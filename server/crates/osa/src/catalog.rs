//! 应用市场（待迁：osa/catalog/）。
use serde_json::Value;
use std::collections::HashMap;
pub fn title(name: &str) -> String {
    format!("@{name}")
}
pub fn public() -> Vec<Value> {
    vec![]
}
pub fn categories() -> Vec<Value> {
    vec![]
}
pub fn builtin(_id: &str) -> Option<Value> {
    None
}
pub fn items() -> HashMap<String, Value> {
    HashMap::new()
}
