//! 聊天软件（待迁：osa/channels/）。
use serde_json::Value;
pub fn on_event(_e: &Value) {}
pub async fn start() {}
pub async fn stop() {}
pub fn chat_removed(_chat_id: i64) {}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
