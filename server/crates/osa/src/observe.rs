//! 待迁：osa/observe.py。
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
    axum::Router::new().route("/api/usage", get(usage)).route("/api/status", get(status))
}


async fn usage() -> Res {
    ok(crate::memory::usage_stats())
}

async fn status() -> Res {
    ok(json!({"mcp": crate::mcp_hub::status_strings(), "mcp_tools": crate::mcp_hub::all_tool_names(), "mcp_confirm": crate::mcp_hub::confirm_list(),
              "routines": crate::routines::list_for(None).iter().map(|r| json!({"id": r["id"], "bot_id": r["bot_id"], "name": r["name"],
                   "enabled": r["enabled"], "trigger": crate::routines::describe(&r["trigger"], None)})).collect::<Vec<_>>()}))
}

