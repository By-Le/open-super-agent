//! 技能（迁移中：osa/skills.py）。
use serde_json::Value;
pub fn parse(_bot_id: i64, _text: &str) -> Option<(Value, String)> {
    None
}
pub fn expand(_skill: &Value, _rest: &str) -> String {
    String::new()
}
pub fn summary(_bot_id: i64) -> String {
    "暂无技能".into()
}
pub fn save(_bot_id: i64, _name: &str, _body: &str, _desc: &str, _source: &str) -> Result<Value, String> {
    Err("Rust 版还没有技能".into())
}
pub fn list_for(_bot_id: i64) -> Vec<Value> {
    vec![]
}
