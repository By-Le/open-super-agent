//! 应用（MCP）（迁移中：osa/mcp_hub.py）。
use serde_json::{Value, json};
pub fn needs_confirm(_tool: &str) -> bool {
    false
}
pub fn public() -> Vec<Value> {
    vec![]
}
pub fn all_apps() -> Vec<String> {
    vec![]
}
pub fn from_yaml(v: &Value) -> Value {
    v.clone()
}
pub async fn start() {}
pub async fn stop() {}
pub fn status_strings() -> Value {
    json!({})
}
pub fn all_tool_names() -> Vec<String> {
    vec![]
}
pub fn confirm_list() -> Vec<String> {
    vec![]
}
