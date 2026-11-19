//! 应用的 OAuth 授权（迁移中：osa/mcp_hub.py 的 OAuth 部分）。现在只用保险库里已有的令牌（Python 版授权过的照样能用）。
use crate::{mcp_hub, vault};
use serde_json::Value;

fn load(name: &str) -> Value {
    vault::get(&mcp_hub::oauth_key(name)).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

pub fn has_tokens(name: &str) -> bool {
    crate::py::truthy(&load(name)["tokens"])
}

pub fn access_token(name: &str) -> Option<String> {
    load(name)["tokens"]["access_token"].as_str().map(String::from)
}

/// 正在等用户在弹窗里授权
pub fn waiting_for_user(_name: &str) -> bool {
    false
}

/// 这次连接是用户点了“登录”发起的
pub fn interactive(_name: &str) -> bool {
    false
}

pub fn cancel_login(_name: &str) {}
