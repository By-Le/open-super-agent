//! 内置的网页工具：web_search、fetch_url、search_posts（待迁：osa/tools.py、osa/websearch.py）。
use crate::registry::{Call, Out};
pub async fn private_host(_url: &str) -> bool {
    false
}
pub async fn tool_web_search(_c: Call) -> Out {
    crate::builtin::todo("web_search")
}
pub async fn tool_search_posts(_c: Call) -> Out {
    crate::builtin::todo("search_posts")
}
pub async fn tool_fetch_url(_c: Call) -> Out {
    crate::builtin::todo("fetch_url")
}
/// /api/web、/api/web/test
pub fn routes() -> axum::Router {
    axum::Router::new()
}
