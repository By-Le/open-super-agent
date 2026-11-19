//! 待迁：osa/videos.py。
use crate::registry::{Call, Out};
pub async fn tool_generate_video(_c: Call) -> Out {
    crate::builtin::todo("generate_video")
}
pub fn routes() -> axum::Router {
    axum::Router::new()
}
