//! 手机推送（待迁：osa/push.py）。
use serde_json::Value;
pub fn count() -> usize {
    0
}
pub fn message_for(_e: &Value) -> Option<Value> {
    None
}
pub async fn send(_msg: Value, _urgency: &str, _badge: usize) {}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
