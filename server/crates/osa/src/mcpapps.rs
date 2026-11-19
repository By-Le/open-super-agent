//! MCP Apps 宿主：应用自带的界面。对应 osa/mcpapps.py（规范和取舍见那里的说明）。

use crate::registry::{self, Tool};
use crate::task::Task;
use crate::{approvals, mcp_hub, py};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

pub const EXT: &str = "io.modelcontextprotocol/ui";
pub const MIME: &str = "text/html;profile=mcp-app";
pub const MAX_VIEWS: usize = 3;
pub const MAX_RESULT: usize = 200_000;
pub const MAX_HTML: usize = 2_000_000;
pub const MAX_DOMAINS: usize = 20;
pub const MODEL_HINT: &str = "\n\n【给你的说明，不要写进答案】用户会在任务结果下面直接看到这个应用的界面，上面就是这些数据。最终答案只写一两句结论，不要贴原始数据，也不要提到界面或这条说明。";

pub fn capability() -> Value {
    json!({EXT: {"mimeTypes": [MIME]}})
}

static VIEWS: LazyLock<Mutex<HashMap<(String, String), (String, String)>>> = LazyLock::new(Default::default);

/// 工具的 _meta → (界面的 ui:// 地址, visibility)
pub fn tool_ui(meta: &Value) -> (Option<String>, Vec<String>) {
    let ui = if meta["ui"].is_object() { meta["ui"].clone() } else { json!({}) };
    let uri = ui["resourceUri"].as_str().filter(|s| !s.is_empty()).or(meta["ui/resourceUri"].as_str());
    let vis = match ui["visibility"].as_array() {
        Some(a) => a.iter().filter_map(|v| v.as_str()).filter(|v| *v == "model" || *v == "app").map(String::from).collect(),
        None => vec!["model".into(), "app".into()],
    };
    (uri.filter(|u| u.starts_with("ui://")).map(String::from), vis)
}

pub fn dump(result: &Value) -> Value {
    let mut o = serde_json::Map::new();
    for k in ["content", "structuredContent", "isError", "_meta"] {
        if let Some(v) = result.get(k).filter(|v| !v.is_null()) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

pub fn view_record(app: &str, tool: &str, uri: &str, args: &Value, result: &Value) -> Option<Value> {
    let rec = json!({"app": app, "tool": tool, "uri": uri, "input": args, "result": dump(result)});
    if py::char_len(&py::dumps_u(&rec)) <= MAX_RESULT { Some(rec) } else { None }
}

static SRC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(https://|wss://)(\*\.)?[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*(:\d{1,5})?(/[A-Za-z0-9._~/%-]*)?$").unwrap());
static LOCAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(http|ws)://(localhost|127\.0\.0\.1)(:\d{1,5})?$").unwrap());

fn domains(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for d in v.as_array().cloned().unwrap_or_default() {
        if let Some(s) = d.as_str().map(|s| s.trim().to_string()) {
            if (SRC.is_match(&s) || LOCAL.is_match(&s)) && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out.truncate(MAX_DOMAINS);
    out
}

pub fn csp(declared: &Value) -> String {
    let d = if declared.is_object() { declared.clone() } else { json!({}) };
    let or_none = |v: Vec<String>| if v.is_empty() { "'none'".to_string() } else { v.join(" ") };
    let res = domains(&d["resourceDomains"]).join(" ");
    let conn = or_none(domains(&d["connectDomains"]));
    let frame = or_none(domains(&d["frameDomains"]));
    let base = or_none(domains(&d["baseUriDomains"]));
    format!(
        "sandbox allow-scripts allow-popups allow-popups-to-escape-sandbox allow-forms; default-src 'none'; script-src 'unsafe-inline' {res}; style-src 'unsafe-inline' {res}; img-src data: blob: {res}; font-src data: {res}; media-src data: blob: {res}; connect-src {conn}; frame-src {frame}; base-uri {base}; form-action 'none'; object-src 'none'"
    )
    .replace("  ", " ")
}

fn session(app: &str) -> Result<Arc<crate::mcp_client::Session>, String> {
    mcp_hub::session_of(app).ok_or_else(|| t!("应用 @{name} 没连接", name = app))
}

fn declared(app: &str) -> Vec<String> {
    registry::by_group().get(app).map(|ts| ts.iter().filter_map(|t| t.meta["mcp"]["ui"].as_str().map(String::from)).collect()).unwrap_or_default()
}

pub async fn view(app: &str, uri: &str) -> Result<(String, String), String> {
    if !declared(app).iter().any(|u| u == uri) {
        return Err(t!("应用 @{name} 没有界面 {uri}", name = app, uri = uri));
    }
    if let Some(hit) = VIEWS.lock().get(&(app.to_string(), uri.to_string())) {
        return Ok(hit.clone());
    }
    let rr = session(app)?.read_resource(uri).await.map_err(|e| e.to_string())?;
    let contents = rr["contents"].as_array().cloned().unwrap_or_default();
    let c = contents.iter().find(|c| c["uri"] == uri).or(contents.first()).cloned().ok_or_else(|| t!("应用 @{name} 没有界面 {uri}", name = app, uri = uri))?;
    let mime_raw = c["mimeType"].as_str().unwrap_or("").to_string();
    let mime = mime_raw.to_lowercase().replace(' ', "");
    if !mime.is_empty() && !mime.starts_with("text/html") {
        return Err(t!("界面 {uri} 不是网页（{mime}）", uri = uri, mime = mime_raw));
    }
    let html = match c["text"].as_str() {
        Some(h) => h.to_string(),
        None => c["blob"].as_str().and_then(crate::util::b64_decode).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default(),
    };
    if html.is_empty() || py::char_len(&html) > MAX_HTML {
        return Err(t!("界面 {uri} 是空的或太大", uri = uri));
    }
    let out = (html, csp(&c["_meta"]["ui"]["csp"]));
    VIEWS.lock().insert((app.into(), uri.into()), out.clone());
    Ok(out)
}

pub fn forget(app: &str) {
    VIEWS.lock().retain(|k, _| k.0 != app);
}

fn tool_of(app: &str, name: &str) -> Result<Arc<Tool>, (u16, String)> {
    let t = registry::by_group()
        .get(app)
        .and_then(|ts| ts.iter().find(|t| t.meta["mcp"]["tool"] == name).cloned())
        .ok_or_else(|| (404, t!("应用 @{app} 没有工具 {tool}", app = app, tool = name)))?;
    if !t.meta["mcp"]["visibility"].as_array().is_some_and(|v| v.iter().any(|x| x == "app")) {
        return Err((403, t!("工具 {tool} 不给界面调用", tool = name)));
    }
    Ok(t)
}

/// 命中确认规则的工具先弹卡片，用户拒绝就报错（403）
pub async fn confirm(bot: Option<&Value>, tool: &str, args: &Value, label: &str) -> Result<Value, (u16, String)> {
    if !approvals::decide(tool, None, args).0 {
        return Ok(args.clone());
    }
    let chat_id = bot.and_then(|b| b["chat_id"].as_i64()).ok_or_else(|| (403, t!("这个操作要确认，但发起它的 Bot 已删除")))?;
    let fake = Task::fake(label, bot.and_then(|b| b["id"].as_i64()).unwrap_or(0), chat_id, label, false);
    let (ok, note, args) = approvals::request(&fake, tool, args, approvals::Req::default()).await;
    if !ok {
        return Err((403, t!("用户拒绝了这个操作。{note}", note = note).trim().to_string()));
    }
    Ok(args)
}

pub async fn call(app: &str, name: &str, args: &Value, bot: Option<&Value>) -> Result<Value, (u16, String)> {
    if !args.is_object() {
        return Err((400, t!("参数要是一个对象")));
    }
    let t = tool_of(app, name)?;
    let args = confirm(bot, &t.name, args, &format!("应用 @{app} 的界面调用 {name}")).await?;
    let s = session(app).map_err(|e| (404, e))?;
    s.call_tool(name, &args).await.map(|r| dump(&r)).map_err(|e| (400, e.to_string()))
}

pub async fn read(app: &str, uri: &str) -> Result<Value, (u16, String)> {
    if uri.is_empty() {
        return Err((400, t!("缺少 uri")));
    }
    session(app).map_err(|e| (404, e))?.read_resource(uri).await.map_err(|e| (400, e.to_string()))
}
