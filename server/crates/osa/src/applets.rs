//! 小应用：Bot 做出来的可交互界面（资料库里带清单 app 的 html 页面）。对应 osa/applets.py（说明见那里）。

use crate::registry::{self, APPLET, Call, Out};
use crate::{bots, mcpapps, providers, py, router, server};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

pub const MAX_TOOLS: usize = 20;
pub const MAX_KEYS: usize = 100;
pub const MAX_VALUE: usize = 32_000;
pub const MAX_DATA: usize = 256_000;
pub const MAX_SAY: usize = 4000;
pub const MAX_CALLS: i64 = 4;

pub const SDK_JS: &str = include_str!("prompts/library/applet_sdk.js");

static RUNNING: LazyLock<Mutex<HashMap<i64, i64>>> = LazyLock::new(Default::default);

/// 检查 Bot 给的清单，不认识的键丢掉。null 或空就是普通页面
pub fn manifest(app: &Value) -> Result<Option<Value>, String> {
    if !py::truthy(app) {
        return Ok(None);
    }
    let Some(o) = app.as_object() else { return Err(t!("app 要是一个对象")) };
    let tools = o.get("tools").filter(|v| py::truthy(v)).cloned().unwrap_or(json!([]));
    let Some(list) = tools.as_array().filter(|a| a.iter().all(|t| t.is_string())) else {
        return Err(t!("app.tools 要是工具名的列表"));
    };
    let mut out: Vec<String> = vec![];
    for t in list.iter().filter_map(|t| t.as_str()).map(|t| t.trim()).filter(|t| !t.is_empty()) {
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out.truncate(MAX_TOOLS);
    Ok(Some(json!({"tools": out, "chat": py::truthy(o.get("chat").unwrap_or(&Value::Null))})))
}

/// 小应用能列进清单的工具：注册表里标了 applet 场合的
pub fn known(tool: &str) -> bool {
    registry::available(tool, APPLET, &json!({})).is_some()
}

/// 合并一次改动：值为 null 的键删掉。返回合并后的全部数据，超限就报错
pub fn check_data(updates: &Value, current: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let Some(u) = updates.as_object() else { return Err(t!("数据要是一个对象")) };
    let mut out = current.clone();
    for (k, v) in u {
        if k.is_empty() || py::char_len(k) > 100 {
            return Err(t!("键要是不超过 100 字的文字"));
        }
        if v.is_null() {
            out.shift_remove(k);
            continue;
        }
        if py::char_len(&py::dumps_u(v)) > MAX_VALUE {
            return Err(t!("「{key}」太大（最多 {n}K 字）", key = k, n = MAX_VALUE / 1000));
        }
        out.insert(k.clone(), v.clone());
    }
    if out.len() > MAX_KEYS {
        return Err(t!("最多 {n} 个键", n = MAX_KEYS));
    }
    if py::char_len(&py::dumps_u(&Value::Object(out.clone()))) > MAX_DATA {
        return Err(t!("数据太大（最多 {n}K 字）", n = MAX_DATA / 1000));
    }
    Ok(out)
}

struct Running(i64);

impl Drop for Running {
    fn drop(&mut self) {
        let mut r = RUNNING.lock();
        if let Some(n) = r.get_mut(&self.0) {
            *n -= 1;
            if *n <= 0 {
                r.remove(&self.0);
            }
        }
    }
}

/// 界面里调一个工具。只认清单里列的；要确认的先弹卡片。出错是 (状态码, 说明)
pub async fn call(page: &Value, tool: &str, args: &Value) -> Result<String, (u16, String)> {
    let allowed = page["app"]["tools"].as_array().is_some_and(|a| a.iter().any(|t| t == tool));
    if !allowed {
        return Err((403, t!("这个小应用不能用工具 {tool}", tool = tool)));
    }
    let tl =
        registry::available(tool, APPLET, &json!({})).ok_or_else(|| (404, t!("现在没有工具 {tool}（应用可能没连接）", tool = tool)))?;
    if !args.is_object() {
        return Err((400, t!("参数要是一个对象")));
    }
    let pid = page["id"].as_i64().unwrap_or(0);
    {
        let mut r = RUNNING.lock();
        let n = r.entry(pid).or_insert(0);
        if *n >= MAX_CALLS {
            return Err((400, t!("这个小应用同时最多 {n} 个调用", n = MAX_CALLS)));
        }
        *n += 1;
    }
    let _guard = Running(pid);
    let bot = page["bot_id"].as_i64().filter(|b| *b != 0).and_then(bots::get);
    let chat_id = bot.as_ref().and_then(|b| b["chat_id"].as_i64()).unwrap_or(0);
    let mut args = args.clone();
    if tl.gated {
        args = mcpapps::confirm(bot.as_ref(), tool, &args, &format!("小应用「{}」调用 {tool}", py::str_of(&page["title"]))).await?;
    }
    let c = Call {
        scope: APPLET,
        bot: bot.clone().unwrap_or(json!({"id": page["bot_id"]})),
        chat_id,
        private_chat_id: chat_id,
        args,
        task: None,
        post: None,
        files: Arc::new(Mutex::new(vec![])),
        apps: Arc::new(Mutex::new(vec![])),
        env: json!({}),
    };
    Ok(match (tl.run)(c).await {
        Out::Text(s) | Out::Shot(s, _) => s,
    })
}

/// 界面替用户给 Bot 发一条消息
pub fn say(page: &Value, text: &str) -> Result<(), (u16, String)> {
    if !py::truthy(&page["app"]["chat"]) {
        return Err((403, t!("这个小应用不能发消息")));
    }
    send(page["bot_id"].as_i64(), text, json!({"id": page["id"], "title": page["title"]}))
}

/// 界面替用户给 Bot 发一条消息，发到 Bot 的私聊里。origin 记在消息上（小应用 {id, title}；应用的界面 {app, title}）
pub fn send(bot_id: Option<i64>, text: &str, origin: Value) -> Result<(), (u16, String)> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err((400, t!("消息不能为空")));
    }
    if py::char_len(&text) > MAX_SAY {
        return Err((400, t!("消息太长（最多 {n} 字）", n = MAX_SAY)));
    }
    let bot = bot_id.filter(|b| *b != 0).and_then(bots::get);
    let chat = bot.as_ref().and_then(|b| b["chat_id"].as_i64()).and_then(bots::chat).ok_or_else(|| (404, t!("发起它的 Bot 已删除")))?;
    tokio::spawn(handle(chat, text, origin));
    Ok(())
}

/// 小应用、应用的界面替用户发的消息（Python 版 server._applet_say）
async fn handle(chat: Value, text: String, applet: Value) {
    let cid = chat["id"].as_i64().unwrap_or(0);
    let bot = bots::ids_of(&chat).first().copied().unwrap_or(0);
    if let Err(e) = router::runtime(bot).handle_user(&text, "", Some(cid), &[], Some(applet.clone())).await {
        tracing::error!("小应用 {} 发消息出错：{e:#}", py::str_of(&applet["id"]));
        server::broadcast(json!({"type": "error", "chat_id": cid, "message": t!("出错了：{e}", e = providers::brief(&e))}));
    }
}

static HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<head\b[^>]*>").unwrap());
static HTML: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<html\b[^>]*>").unwrap());

/// 把 SDK 内联到页面开头（<head> 里，没有就放最前面）
pub fn inject(html: &str) -> String {
    let tag = format!("<script>{SDK_JS}</script>");
    for re in [&*HEAD, &*HTML] {
        if let Some(m) = re.find(html) {
            return format!("{}{tag}{}", &html[..m.end()], &html[m.end()..]);
        }
    }
    format!("{tag}{html}")
}
