use std::{error::Error, sync::Arc};

use chrono::Utc;
use futures::{
    channel::{mpsc, oneshot},
    lock::Mutex,
    SinkExt,
};
use libp2p::PeerId;
use num_bigint::BigUint;

use crate::{
    network::{
        messages::{
            MessageCommand, MessagePayload, NetworkMessage, Object, ObjectKind, UnencryptedPubkey,
        },
        node::worker::NodeWorker,
    },
    pow,
    repositories::{
        address::AddressRepositorySync, inventory::InventoryRepositorySync,
        message::MessageRepositorySync,
    },
};

use super::worker::WorkerCommand;

pub struct Handler {
    address_repo: Box<AddressRepositorySync>,
    inventory_repo: Arc<Mutex<Box<InventoryRepositorySync>>>,
    message_repo: Box<MessageRepositorySync>,
    requested_objects: Vec<String>, // TODO periodically request missing object from every connection we have
    worker_event_sender: mpsc::Sender<WorkerCommand>,
    pubkey_notifier_sink: mpsc::Sender<String>,
}

impl Handler {
    pub fn new(
        address_repo: Box<AddressRepositorySync>,
        inventory_repo: Box<InventoryRepositorySync>,
        message_repo: Box<MessageRepositorySync>,
        worker_event_sender: mpsc::Sender<WorkerCommand>,
        pubkey_notifier_sink: mpsc::Sender<String>,
    ) -> Handler {
        Handler {
            address_repo,
            inventory_repo: Arc::new(Mutex::new(inventory_repo)),
            message_repo,
            requested_objects: Vec::new(),
            worker_event_sender,
            pubkey_notifier_sink,
        }
    }

    pub async fn handle_message(&mut self, msg: NetworkMessage) -> Option<NetworkMessage> {
        match msg.command {
            MessageCommand::GetData => Some(self.handle_get_data(msg.payload).await),
            MessageCommand::Inv => self.handle_inv(msg.payload).await,
            MessageCommand::ReqInv => Some(self.handle_get_inv_message(msg.payload).await),
            MessageCommand::Objects => {
                self.handle_objects(msg.payload).await;
                None
            }
        }
    }

    async fn handle_get_inv_message(&self, _: MessagePayload) -> NetworkMessage {
        let inv = self
            .inventory_repo
            .lock()
            .await
            .get()
            .await
            .expect("Inventory repo not to fail");
        NetworkMessage {
            command: MessageCommand::Inv,
            payload: MessagePayload::Inv { inventory: inv },
        }
    }

    async fn handle_inv(&self, payload: MessagePayload) -> Option<NetworkMessage> {
        let mut inv = if let MessagePayload::Inv { inventory } = payload {
            inventory
        } else {
            Vec::new()
        };
        self.inventory_repo
            .lock()
            .await
            .get_missing_objects(&mut inv)
            .await
            .expect("Repo not to fail");
        if !inv.is_empty() {
            return Some(NetworkMessage {
                command: MessageCommand::GetData,
                payload: MessagePayload::GetData { inventory: inv },
            });
        }
        None
    }

    async fn handle_objects(&mut self, payload: MessagePayload) {
        let objects: Vec<Object> = if let MessagePayload::Objects { objects } = payload {
            objects
        } else {
            log::warn!("incorrent payload passed to handle_object function");
            return;
        };

        for obj in objects {
            let hash_str = bs58::encode(&obj.hash).into_string();
            self.requested_objects.retain(|v| *v == hash_str);

            if self
                .inventory_repo
                .lock()
                .await
                .get_object(hash_str.clone())
                .await
                .unwrap()
                .is_some()
            {
                log::debug!("object {hash_str} is already in the inventory, skipping it");
                continue;
            }

            let target = pow::get_pow_target(
                &obj,
                pow::NETWORK_MIN_NONCE_TRIALS_PER_BYTE,
                pow::NETWORK_MIN_EXTRA_BYTES,
            );
            let pow_check_res =
                pow::check_pow(target, BigUint::from_bytes_be(&obj.nonce), obj.hash.clone());
            if pow_check_res.is_err() {
                log::warn!(
                    "object with hash {:?} has invalid nonce! skipping it",
                    bs58::encode(obj.hash).into_string()
                );
                continue;
            }

            self.inventory_repo
                .lock()
                .await
                .store_object(hash_str, obj.clone())
                .await
                .expect("repo not to fail");

            self.offer_inv().await.expect("inv msg to be published");

            let handler_result = match &obj.kind {
                ObjectKind::Msg { encrypted: _ } => self.handle_msg_object(obj.clone()).await,
                ObjectKind::Broadcast {
                    tag: _,
                    encrypted: _,
                } => Err("we don't support broadcast at the moment, skipping it...".into()),
                ObjectKind::Getpubkey { tag: _ } => {
                    self.handle_get_pubkey_object(obj.clone()).await
                }
                ObjectKind::Pubkey {
                    tag: _,
                    encrypted: _,
                } => self.handle_pubkey_object(obj.clone()).await,
            };
            if let Err(r) = handler_result {
                log::error!("{:?}", r.to_string());
                continue;
            }
        }
    }

    async fn handle_pubkey_object(&mut self, object: Object) -> Result<(), Box<dyn Error>> {
        let (tag, encrypted) = if let ObjectKind::Pubkey { tag, encrypted } = object.kind {
            (tag, encrypted)
        } else {
            return Err("incorrent object kind!".into());
        };

        let tag_str = bs58::encode(&tag).into_string();
        let result = self
            .address_repo
            .get_by_ripe_or_tag(tag_str.clone())
            .await
            .expect("repo not to fail");
        let decryption_result = match result {
            Some(a) => {
                let dec_result = ecies::decrypt(&a.public_decryption_key.serialize(), &encrypted);
                dec_result
            }
            None => return Ok(()), // just ignore it
        };
        let data: UnencryptedPubkey = match decryption_result {
            Ok(d) => serde_cbor::from_slice(&d).expect("pubkey msg in correct format!"),
            Err(_) => return Ok(()), // just ignore it
        };

        self.address_repo
            .update_public_keys(
                tag_str.clone(),
                ecies::PublicKey::parse_slice(&data.public_signing_key, None)
                    .expect("public signing key parses correctly"),
                ecies::PublicKey::parse_slice(&data.public_encryption_key, None)
                    .expect("public encryption key parses correctly"),
            )
            .await
            .expect("repo not to fail");

        self.pubkey_notifier_sink.send(tag_str).await.unwrap();

        Ok(())
    }

    async fn handle_get_pubkey_object(&mut self, object: Object) -> Result<(), Box<dyn Error>> {
        let tag = if let ObjectKind::Getpubkey { tag } = object.kind {
            tag
        } else {
            return Err("incorrect object kind!".into());
        };
        let identities = self
            .address_repo
            .get_identities()
            .await
            .expect("repo not to fail");
        for i in identities {
            if i.tag == tag {
                log::debug!("someone requested our pubkey! sending it out...");
                // FIXME only send pubkey if it wasn't sent in the last 28 days
                let ttl = chrono::Duration::days(28);
                let expires = Utc::now() + ttl;
                let serialized_psk = i.public_signing_key.unwrap().serialize();
                let serialized_pek = i.public_encryption_key.unwrap().serialize();

                let unencrypted_pubkey = UnencryptedPubkey {
                    behaviour_bitfield: 0,
                    public_signing_key: serialized_psk.to_vec(),
                    public_encryption_key: serialized_pek.to_vec(),
                };

                let obj = Object::with_signing(
                    &i,
                    ObjectKind::Pubkey {
                        tag: i.tag.clone(),
                        encrypted: NodeWorker::serialize_and_encrypt_payload(
                            unencrypted_pubkey,
                            &i.public_decryption_key,
                        ),
                    },
                    expires,
                );
                obj.do_proof_of_work(self.worker_event_sender.clone());
            }
        }

        Ok(())
    }

    async fn handle_msg_object(&mut self, object: Object) -> Result<(), Box<dyn Error>> {
        let encrypted = if let ObjectKind::Msg { encrypted } = object.kind {
            encrypted
        } else {
            return Err("incorrect object kind!".into());
        };
        let identities = self
            .address_repo
            .get_identities()
            .await
            .expect("Address repo not to fail");
        for i in identities {
            let decryption_result =
                ecies::decrypt(&i.private_encryption_key.unwrap().serialize(), &encrypted);
            if let Ok(msg) = decryption_result {
                log::debug!("message object successfully decrypted! saving it...");
                match serde_cbor::from_slice(msg.as_slice()) {
                    Ok(msg) => {
                        self.message_repo
                            .save(
                                bs58::encode(&object.hash).into_string(),
                                msg,
                                object.signature.clone(),
                            )
                            .await
                            .expect("repo not to fail");
                    }
                    Err(e) => {
                        log::error!("received malformed message! skipping it");
                        return Err(Box::new(e));
                    }
                }
            } else {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn offer_inv(&mut self) -> Result<(), Box<dyn Error + Send>> {
        let inventory = self
            .inventory_repo
            .lock()
            .await
            .get()
            .await
            .expect("repo not to be failed");

        let msg = NetworkMessage {
            command: MessageCommand::Inv,
            payload: MessagePayload::Inv { inventory },
        };
        let (sender, receiver) = oneshot::channel();
        self.worker_event_sender
            .send(WorkerCommand::BroadcastMsgByPubSub { sender, msg })
            .await
            .expect("receiver not to be dropped");
        receiver.await.unwrap()
    }

    async fn handle_get_data(&self, payload: MessagePayload) -> NetworkMessage {
        let inv = if let MessagePayload::GetData { inventory } = payload {
            inventory
        } else {
            Vec::new()
        };

        let mut objects: Vec<Object> = Vec::new();

        for hash in inv {
            if let Some(obj) = self
                .inventory_repo
                .lock()
                .await
                .get_object(hash)
                .await
                .expect("Repository not to fail")
            {
                objects.push(obj);
            }
        }

        log::debug!("requested {} objects from this node", objects.len());

        NetworkMessage {
            command: MessageCommand::Objects,
            payload: MessagePayload::Objects { objects },
        }
    }
}
