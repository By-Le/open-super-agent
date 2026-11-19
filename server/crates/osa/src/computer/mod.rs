//! 电脑（待迁：osa/computer/，第 2 步）。
pub mod tools;
use crate::registry::Out;
use crate::task::Task;
use serde_json::Value;
use std::sync::Arc;

pub async fn desk_release(_key: &str) {}
pub async fn look_for_router() -> Out {
    "电脑现在没开（已暂停或还没创建）。需要的话派任务，任务会自动开机。".into()
}
pub async fn direct(_bot_id: i64, _bot_name: &str, _chat_id: i64, _name: &str, _args: &Value) -> Out {
    "电脑现在用不了（Rust 版还没有电脑）。".into()
}
pub fn link_rows() -> Vec<Value> {
    vec![]
}
pub fn pick_mac(name: &str) -> (Option<Value>, String) {
    (None, format!("没派发：没有叫「{name}」的 Mac。用户登记过的 Mac：没有"))
}
pub async fn use_mac(task: &Arc<Task>) -> String {
    crate::t!("「{name}」已经移除了，没有做", name = task.mac_name.clone())
}
pub async fn with_task_pc<F: std::future::Future>(f: F) -> F::Output {
    f.await
}
pub async fn shutdown() {}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
