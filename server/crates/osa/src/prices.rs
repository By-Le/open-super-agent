//! 单价（待迁：osa/prices.py）。
use serde_json::{Value, json};
pub fn cost(_provider: &str, _model: &str, _hit: i64, _miss: i64, _out: i64, _peak: bool) -> f64 {
    0.0
}
pub fn models_of(_provider: &str) -> Vec<String> {
    vec![]
}
pub fn annotate(_provider: &str, models: &[String]) -> Vec<Value> {
    models.iter().map(|m| json!({"id": m})).collect()
}
pub fn lookup(_provider: &str, _model: &str) -> Value {
    json!({})
}
