//! 定时任务和事件（待迁：osa/routines.py、events.py）。
use serde_json::Value;
pub fn list_for(_bot_id: Option<i64>) -> Vec<Value> {
    vec![]
}
pub fn noun(_r: &Value) -> String {
    "定时任务".into()
}
pub fn describe(_t: &Value, _lang: Option<&str>) -> String {
    String::new()
}
pub fn is_quiet(_result: &str) -> bool {
    false
}
pub fn import_legacy(_bot_id: i64) {}
pub async fn scheduler() {}
use crate::registry::{Call, Out};
pub async fn tool_create(_c: Call) -> Out {
    crate::builtin::todo("create_routine")
}
pub async fn tool_create_event(_c: Call) -> Out {
    crate::builtin::todo("create_event")
}
pub async fn tool_list(_c: Call) -> Out {
    crate::builtin::todo("list_routines")
}
pub async fn tool_update(_c: Call) -> Out {
    crate::builtin::todo("update_routine")
}
pub async fn tool_delete(_c: Call) -> Out {
    crate::builtin::todo("delete_routine")
}
/// /api/routines…、/api/event-sources、/api/bots/{id}/routines、/hooks/{name}
pub fn routes() -> axum::Router {
    axum::Router::new()
}
