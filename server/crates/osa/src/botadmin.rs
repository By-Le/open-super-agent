//! 主 Bot 管理所有 Bot（待迁：osa/botadmin.py）。
use crate::router::Router;
use crate::task::Reply;
use serde_json::Value;
use std::sync::Arc;

pub const TOOLS: [&str; 4] = ["get_bot", "create_bot", "update_bot", "delete_bot"];

pub fn tools_for(bot: &Value) -> Vec<Value> {
    let s = crate::assets::schemas();
    let k = if bot["is_primary"] == true { "botadmin_primary" } else { "botadmin_other" };
    s[k].as_array().cloned().unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
pub async fn call(_r: &Arc<Router>, _name: &str, _bot: &Value, _chat_id: i64, _private: i64, _args: &Value, _reply: Option<Reply>, _depth: i64) -> String {
    "Rust 版还没有这个工具".into()
}

/// 删除一个 Bot（接口 DELETE /api/bots/{id}）
pub async fn remove(bot_id: i64) -> Result<(), String> {
    crate::approvals::cancel_for_bot(bot_id);
    for t in crate::router::tasks() {
        if t.bot_id == bot_id && t.status() == "running" {
            crate::router::cancel(t.id, &crate::t!("Bot 已删除"));
        }
    }
    crate::bots::delete(bot_id)?;
    crate::memories::delete_for_bot(bot_id);
    crate::router::forget_runtime(bot_id);
    Ok(())
}
