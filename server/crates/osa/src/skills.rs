//! 技能（迁移中：osa/skills.py）。
#[allow(unused_imports)]
use crate::web::{Body, Res, ok};
#[allow(unused_imports)]
use crate::server::broadcast;
#[allow(unused_imports)]
use axum::extract::Path;
#[allow(unused_imports)]
use axum::http::HeaderMap;
#[allow(unused_imports)]
use axum::routing::{get, post};
#[allow(unused_imports)]
use serde_json::json;
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
pub async fn tool_use_skill(_c: crate::registry::Call) -> crate::registry::Out {
    crate::builtin::todo("use_skill")
}
/// /api/bots/{id}/skills…、/api/skills/…、/api/shared/skills/…



pub fn routes() -> axum::Router {
    axum::Router::new().route("/api/bots/{bot_id}/skills", get(list_skills))
}


async fn list_skills(Path(bot_id): Path<i64>) -> Res {
    crate::server::bot_or_404(bot_id)?;
    ok(json!(crate::skills::list_for(bot_id)))
}

