//! 演示一遍（待迁：osa/teach.py）。
use serde_json::Value;
pub fn public() -> Value {
    Value::Null
}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
