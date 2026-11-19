//! 界面上可修改的设置（迁移中：osa/settings.py）。
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
pub fn apply() {
    crate::providers::apply();
}
pub fn onboarding() -> bool {
    if crate::db::exists("select 1 from settings where key='onboarding'", []) {
        return false;
    }
    !crate::db::exists("select 1 from messages where role='user' limit 1", [])
}
pub fn finish_onboarding() {
    crate::memory::set_setting("onboarding", &json!({"done": crate::py::now()}));
}
pub fn public() -> Value {
    json!({"language": crate::i18n::lang()})
}
pub async fn update(_d: &Value) -> Result<Value, String> {
    Err("Rust 版还没有这个功能：改设置".into())
}




pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/onboarding/done", post(onboarding_done))
}


async fn get_settings() -> Res {
    ok(crate::settings::public())
}

async fn put_settings(Body(d): Body) -> Res {
    let out = crate::settings::update(&d).await?;
    broadcast(json!({"type": "settings_changed"}));
    ok(out)
}

async fn onboarding_done() -> Res {
    crate::settings::finish_onboarding();
    ok(json!({"ok": true}))
}

