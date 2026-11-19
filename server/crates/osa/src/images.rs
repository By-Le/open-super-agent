//! 待迁：osa/images.py。
use crate::registry::{Call, Out};
pub async fn tool_generate_image(_c: Call) -> Out {
    crate::builtin::todo("generate_image")
}
pub fn routes() -> axum::Router {
    axum::Router::new()
}
