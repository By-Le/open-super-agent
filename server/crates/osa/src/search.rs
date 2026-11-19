//! 按意思检索（待迁：osa/search.py）。
use serde_json::Value;
pub async fn search(_text: &str, _bot_id: Option<i64>, _kinds: &[&str], _limit: usize, _subs: &[&str]) -> Vec<Value> {
    vec![]
}
pub async fn tool_search_memory(_c: crate::registry::Call) -> crate::registry::Out {
    crate::builtin::todo("search_memory")
}
/// /api/search、/api/search/info
pub fn routes() -> axum::Router {
    axum::Router::new()
}
