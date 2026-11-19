//! 应用市场和应用的接口：/api/market、/api/apps、/oauth、/api/vault、/api/rules、/api/mcp-apps（待迁：osa/server.py 里那一段）。
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



pub fn routes() -> axum::Router {
    axum::Router::new().route("/api/apps", get(list_apps))
}


async fn list_apps(h: HeaderMap) -> Res {
    let mut o = json!({});
    for (k, v) in crate::server::apps_out(&h) {
        o[k] = v;
    }
    ok(o)
}

