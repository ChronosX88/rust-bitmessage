use std::error::Error;

use async_trait::async_trait;

use crate::network::messages::UnencryptedMsg;

use super::sqlite::models;

#[async_trait]
pub trait MessageRepository {
    /// Save message in repository
    async fn save(&mut self, hash: String, msg: UnencryptedMsg) -> Result<(), Box<dyn Error>>;

    /// Get all messages in repository
    async fn get_messages(&self) -> Result<Vec<models::Message>, Box<dyn Error>>;
}

pub type MessageRepositorySync = dyn MessageRepository + Sync + Send;
