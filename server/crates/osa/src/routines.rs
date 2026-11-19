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
