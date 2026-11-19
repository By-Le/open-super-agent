//! 应用（MCP server）：添加、启用和停用，工具交给 Worker 使用。对应 osa/mcp_hub.py（说明见那里）。
//!
//! 每个应用一个后台任务：连上、登记工具、等停用信号、清理。工具名格式为 <应用名>__<工具>。

use crate::db::{self, RowExt};
use crate::i18n::Later;
use crate::mcp_client::{McpError, Session, Transport};
use crate::registry::{self, APPLET, Call, Context, Out, Tool, WORKER};
use crate::{approvals, config, mcp_oauth, mcpapps, py, security, server, vault};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::Regex;
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::watch;

pub fn migrate() {
    db::batch("create table if not exists apps(name text primary key, config text, enabled integer, created real)");
}

pub const KEYS: [&str; 12] = ["catalog", "type", "command", "args", "env", "url", "headers", "oauth", "oauth_redirect", "deny", "confirm", "owned"];

static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]{0,23}$").unwrap());

struct Running {
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    generation: u64,
}

#[derive(Default)]
struct State {
    ctx: HashMap<String, Arc<Context>>,
    sessions: HashMap<String, Arc<Session>>,
    status: HashMap<String, Later>,
    confirm: HashSet<String>,
    running: HashMap<String, Running>,
    connected: HashSet<String>,
    needs_login: HashSet<String>,
    reconnected: HashMap<String, f64>,
}

static ST: LazyLock<Mutex<State>> = LazyLock::new(Default::default);
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn session_of(name: &str) -> Option<Arc<Session>> {
    ST.lock().sessions.get(name).cloned()
}

pub fn needs_confirm(tool: &str) -> bool {
    ST.lock().confirm.contains(tool)
}

pub fn confirm_list() -> Vec<String> {
    let mut v: Vec<String> = ST.lock().confirm.iter().cloned().collect();
    v.sort();
    v
}

/// 测试、确认卡片用：把一个工具加进 / 移出默认要确认的集合
pub fn set_confirm(tool: &str, on: bool) {
    let mut s = ST.lock();
    if on {
        s.confirm.insert(tool.into());
    } else {
        s.confirm.remove(tool);
    }
}

pub fn status_strings() -> Value {
    let s = ST.lock();
    json!(s.status.iter().map(|(k, v)| (k.clone(), json!(v.text()))).collect::<serde_json::Map<_, _>>())
}

pub fn status_text(name: &str) -> String {
    ST.lock().status.get(name).map(|l| l.text()).unwrap_or_default()
}

pub fn set_status(name: &str, l: Later) {
    ST.lock().status.insert(name.into(), l);
}

pub fn is_connected(name: &str) -> bool {
    ST.lock().connected.contains(name)
}

pub fn mark_needs_login(name: &str, on: bool) {
    let mut s = ST.lock();
    if on {
        s.needs_login.insert(name.into());
    } else {
        s.needs_login.remove(name);
    }
}

pub fn needs_login(name: &str) -> bool {
    ST.lock().needs_login.contains(name)
}

// ---------- 存储 ----------
/// config.yaml 里 mcp_servers 的一项 → 应用配置
pub fn from_yaml(s: &Value) -> Value {
    if py::truthy(&s["url"]) {
        return json!({"type": "http", "url": s["url"], "headers": s.get("headers").cloned().unwrap_or(json!({})), "oauth": py::truthy(&s["oauth"]),
                      "deny": s.get("deny").cloned().unwrap_or(json!([])), "confirm": s.get("confirm").cloned().unwrap_or(json!("auto"))});
    }
    let env: serde_json::Map<String, Value> = s["env_from"].as_array().map(|a| a.iter().map(|k| (py::str_of(k), json!(format!("env:{}", py::str_of(k))))).collect()).unwrap_or_default();
    json!({"command": s.get("command").cloned().unwrap_or(json!("")), "args": s.get("args").cloned().unwrap_or(json!([])), "env": env,
           "deny": s.get("deny").cloned().unwrap_or(json!([])), "confirm": s.get("confirm").cloned().unwrap_or(json!("auto"))})
}

fn seed() {
    let have: HashSet<String> = db::rows("select name from apps", []).into_iter().map(|r| r.s(0)).collect();
    if let Some(m) = config::get("mcp_servers").as_object() {
        for (name, s) in m {
            if have.contains(name) {
                continue;
            }
            db::exec("insert into apps values(?,?,?,?)", params![name, py::dumps(&from_yaml(s), true), py::truthy(&s["enabled"]) as i64, py::now()]);
        }
    }
}

pub fn all_apps_map() -> IndexMap<String, Value> {
    db::rows("select name, config, enabled from apps order by created", [])
        .into_iter()
        .map(|r| {
            let mut c = r.j(1);
            c["enabled"] = json!(r.i(2) != 0);
            (r.s(0), c)
        })
        .collect()
}

pub fn all_apps() -> Vec<String> {
    all_apps_map().keys().cloned().collect()
}

pub fn get(name: &str) -> Option<Value> {
    all_apps_map().get(name).cloned()
}

fn secret_free(v: &Value) -> Value {
    match v.as_str() {
        Some(s) if s.starts_with("vault:") => json!(t!("（保险库：{name}）", name = &s[6..])),
        _ => v.clone(),
    }
}

pub fn state(name: &str, a: &Value) -> &'static str {
    let s = ST.lock();
    if s.connected.contains(name) {
        return "connected";
    }
    if a["enabled"] != true {
        return "off";
    }
    if mcp_oauth::waiting_for_user(name) {
        return "login";
    }
    if s.needs_login.contains(name) {
        return "needs_login";
    }
    if s.running.contains_key(name) {
        return "connecting";
    }
    "error"
}

pub fn public() -> Vec<Value> {
    let mut out = vec![];
    for (n, a) in all_apps_map() {
        let tools = tools_of(&n);
        let confirm_tools: Vec<String> = {
            let s = ST.lock();
            let mut v: Vec<String> = s.confirm.iter().filter_map(|t| t.strip_prefix(&format!("{n}__")).map(String::from)).collect();
            v.sort();
            v
        };
        let st = state(&n, &a);
        out.push(json!({
            "name": n, "catalog": a["catalog"].as_str().unwrap_or(""), "type": a["type"].as_str().filter(|s| !s.is_empty()).unwrap_or("stdio"),
            "command": a.get("command").cloned().unwrap_or(json!("")), "args": a.get("args").cloned().unwrap_or(json!([])),
            "env": a.get("env").cloned().unwrap_or(json!({})), "url": secret_free(&a.get("url").cloned().unwrap_or(json!(""))),
            "headers": a["headers"].as_object().map(|h| h.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
            "oauth": py::truthy(&a["oauth"]), "deny": a.get("deny").cloned().unwrap_or(json!([])), "confirm": a.get("confirm").cloned().unwrap_or(json!("auto")),
            "enabled": a["enabled"], "status": status_text(&n), "state": st, "tools": tools, "connected": is_connected(&n),
            "confirm_tools": confirm_tools,
        }));
    }
    out
}

pub fn save(name: &str, data: &Value) -> Result<Value, String> {
    if !NAME.is_match(name) {
        return Err(t!("应用名只能用小写字母、数字、- 和 _，以字母开头，最多 24 个字符"));
    }
    let old = get(name).unwrap_or(json!({}));
    let mut cfg = serde_json::Map::new();
    for k in KEYS {
        if let Some(v) = data.get(k).or_else(|| old.get(k)) {
            cfg.insert(k.into(), v.clone());
        }
    }
    if data["oauth"] == true && old["oauth"].is_object() {
        cfg.insert("oauth".into(), old["oauth"].clone());
    }
    cfg.entry("deny").or_insert(json!([]));
    cfg.entry("confirm").or_insert(json!("auto"));
    let kind = cfg.get("type").and_then(|v| v.as_str()).unwrap_or("stdio").to_string();
    if kind == "http" || kind == "sse" {
        let url = cfg.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if url.is_empty() {
            return Err(t!("缺少应用地址"));
        }
        cfg.insert("url".into(), json!(url));
        if cfg.get("headers").is_some_and(|h| !h.is_object()) {
            return Err(t!("headers 要是对象"));
        }
    } else {
        let cmd = cfg.get("command").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        cfg.insert("command".into(), json!(cmd));
        cfg.entry("args").or_insert(json!([]));
        cfg.entry("env").or_insert(json!({}));
        if cmd.is_empty() {
            return Err(t!("缺少启动命令"));
        }
        if !cfg["args"].is_array() || !cfg["env"].is_object() {
            return Err(t!("args 要是列表，env 要是对象"));
        }
    }
    let enabled = match data.get("enabled") {
        Some(v) => py::truthy(v),
        None => old.get("enabled").map(py::truthy).unwrap_or(true),
    };
    let cfg = Value::Object(cfg);
    if enabled {
        security::check_app(name, &cfg)?;
    }
    db::exec(
        "insert or replace into apps values(?,?,?,coalesce((select created from apps where name=?),?))",
        params![name, py::dumps(&cfg, true), enabled as i64, name, py::now()],
    );
    let mut out = cfg;
    out["enabled"] = json!(enabled);
    Ok(out)
}

pub fn oauth_key(name: &str) -> String {
    format!("OAUTH_{}", name.to_uppercase().replace('-', "_"))
}

pub fn remove(name: &str) {
    let a = get(name).unwrap_or(json!({}));
    for r in a["owned"].as_array().cloned().unwrap_or_default() {
        vault::delete(&py::str_of(&r));
    }
    vault::delete(&oauth_key(name));
    db::exec("delete from apps where name=?", params![name]);
    let mut s = ST.lock();
    s.status.remove(name);
    s.needs_login.remove(name);
}

// ---------- 密钥 ----------
static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{(vault|env):([A-Za-z_][A-Za-z0-9_]*)\}").unwrap());

pub fn resolve(v: &Value, missing: &mut Vec<String>) -> String {
    let mut s = py::str_of(v);
    if s.starts_with("vault:") || s.starts_with("env:") {
        s = format!("{{{s}}}");
    }
    let mut miss = vec![];
    let out = REF
        .replace_all(&s, |c: &regex::Captures| {
            let val = if &c[1] == "vault" { vault::get(&c[2]) } else { std::env::var(&c[2]).ok() };
            match val.filter(|v| !v.is_empty()) {
                Some(v) => v,
                None => {
                    miss.push(c[0][1..c[0].len() - 1].to_string());
                    String::new()
                }
            }
        })
        .into_owned();
    missing.extend(miss);
    out
}

fn env_of(a: &Value) -> Result<HashMap<String, String>, String> {
    let mut missing = vec![];
    let env: HashMap<String, String> = a["env"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), resolve(v, &mut missing))).collect()).unwrap_or_default();
    if !missing.is_empty() {
        return Err(t!("缺少密钥：{names}", names = missing.join(", ")));
    }
    Ok(env)
}

fn needs_confirm_tool(a: &Value, tool: &Value) -> bool {
    approvals::default_confirm(&a.get("confirm").cloned().unwrap_or(json!("auto")), tool["name"].as_str().unwrap_or(""), tool["annotations"]["readOnlyHint"].as_bool())
}

fn parallel_of(name: &str, a: &Value, tool: &Value, ui: bool) -> usize {
    if ui || !approvals::read_only(tool["name"].as_str().unwrap_or(""), tool["annotations"]["readOnlyHint"].as_bool(), name) {
        return 0;
    }
    if matches!(a["type"].as_str(), Some("http") | Some("sse")) { 3 } else { 1 }
}

fn changed() {
    server::broadcast(json!({"type": "apps_changed"}));
}

fn transport(name: &str, a: &Value) -> Result<Transport, String> {
    security::check_app(name, a)?;
    let kind = a["type"].as_str().unwrap_or("stdio");
    if kind == "http" || kind == "sse" {
        let mut missing = vec![];
        let url = resolve(&a["url"], &mut missing);
        let headers: HashMap<String, String> = a["headers"].as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), resolve(v, &mut missing))).collect()).unwrap_or_default();
        if !missing.is_empty() {
            return Err(t!("缺少密钥：{names}", names = missing.join(", ")));
        }
        let auth = if py::truthy(&a["oauth"]) { Some(mcp_oauth::provider(name, a, &url)?) } else { None };
        return Ok(if kind == "sse" { Transport::Sse { url, headers, auth } } else { Transport::Http { url, headers, auth } });
    }
    let mut env: HashMap<String, String> = std::env::vars().filter(|(k, _)| k != "OSA_PASSWORD").collect();
    env.extend(env_of(a)?);
    let mut missing = vec![];
    let args: Vec<String> = a["args"].as_array().map(|x| x.iter().map(|v| {
        let s = py::str_of(v);
        if s.contains("vault:") || s.contains("env:") { resolve(v, &mut missing) } else { s }
    }).collect()).unwrap_or_default();
    if !missing.is_empty() {
        return Err(t!("缺少密钥：{names}", names = missing.join(", ")));
    }
    if name == "filesystem" || args.join(" ").contains("server-filesystem") {
        let _ = std::fs::create_dir_all(security::workspace());
    }
    Ok(Transport::Stdio { command: a["command"].as_str().unwrap_or("").to_string(), args, env })
}

fn drop_tools(name: &str) {
    let ctx = ST.lock().ctx.remove(name);
    if let Some(c) = ctx {
        c.dispose();
    }
    ST.lock().confirm.retain(|k| !k.starts_with(&format!("{name}__")));
}

pub fn tools_of(name: &str) -> Vec<String> {
    let mut v: Vec<String> = registry::by_group().get(name).map(|ts| ts.iter().map(|t| t.name.split_once("__").map(|x| x.1.to_string()).unwrap_or_default()).collect()).unwrap_or_default();
    v.sort();
    v
}

pub fn all_tool_names() -> Vec<String> {
    let mut v: Vec<String> = registry::by_group().values().flatten().map(|t| t.name.clone()).collect();
    v.sort();
    v
}

fn make_run(name: String, tool: String, full: String, ui: Option<String>, session: Arc<Session>, stop: watch::Receiver<bool>) -> registry::RunFn {
    Arc::new(move |c: Call| -> BoxFuture<'static, Out> {
        let (name, tool, full, ui, session, stop) = (name.clone(), tool.clone(), full.clone(), ui.clone(), session.clone(), stop.clone());
        Box::pin(async move {
            let result = match session.call_tool(&tool, &c.args).await {
                Ok(r) => r,
                Err(e) => {
                    let recent = py::now() - ST.lock().reconnected.get(&name).copied().unwrap_or(0.0) < 60.0;
                    if !e.lost() || *stop.borrow() || recent {
                        return Out::Text(format!("工具出错：{e}"));
                    }
                    ST.lock().reconnected.insert(name.clone(), py::now());
                    let _ = connect(&name, 150.0).await;
                    let again = registry::get(&full).filter(|t| t.owner == format!("mcp:{name}"));
                    let Some(again) = again else {
                        let hint = if needs_login(&name) { "，请用户到应用市场重新登录" } else { "" };
                        return Out::Text(format!("工具出错：应用 @{name} 连接断开：{}{hint}", status_text(&name)));
                    };
                    return (again.run)(c).await;
                }
            };
            let mut text = crate::mcp_client::text_of(&result);
            if let Some(ui) = &ui {
                if c.scope == WORKER && c.env.get("pages").is_none_or(py::truthy) && c.apps.lock().len() < mcpapps::MAX_VIEWS {
                    if let Some(rec) = mcpapps::view_record(&name, &tool, ui, &c.args, &result) {
                        c.apps.lock().push(rec);
                        text += mcpapps::MODEL_HINT;
                    }
                }
            }
            Out::Text(text)
        })
    })
}

async fn serve(name: String, a: Value, ready: tokio::sync::oneshot::Sender<Result<(), String>>, mut stop: watch::Receiver<bool>, wait_s: f64, done: watch::Sender<bool>, generation: u64) {
    let mut ready = Some(ready);
    let mut connected: Option<Later> = None;
    let r: Result<(), (Later, bool)> = async {
        let tr = transport(&name, &a).map_err(|e| (Later::new("连接失败：{error}", &[("error", json!(py::head(&e, 300)))]), false))?;
        let session = Session::connect(tr, mcpapps::capability(), Duration::from_secs_f64(wait_s.max(1.0))).await.map_err(|e| match e {
            // OAuth 要用户登录授权（还没授权、授权过期、没完成、取消了）
            McpError::Unauthorized(m) if py::truthy(&a["oauth"]) => (Later::plain(&m), true),
            McpError::Transport(m) if m == "timeout" => (Later::plain("连接超时"), false),
            e if e.to_string().contains("Token exchange failed") => {
                let s = e.to_string();
                let detail = s.split_once(": ").map(|x| x.1.to_string()).unwrap_or(s);
                (Later::new("登录后没换到令牌，检查一下客户端 ID、密钥和回调地址：{error}", &[("error", json!(py::head(&detail, 200)))]), false)
            }
            e => (Later::new("连接失败：{error}", &[("error", json!(py::head(&e.to_string(), 300)))]), false),
        })?;
        let tools = session.list_tools().await.map_err(|e| (Later::new("连接失败：{error}", &[("error", json!(py::head(&e.to_string(), 300)))]), false))?;
        let deny: Vec<String> = a["deny"].as_array().map(|d| d.iter().map(|w| py::str_of(w).to_lowercase()).collect()).unwrap_or_default();
        drop_tools(&name);
        let ctx = Arc::new(Context::new(&format!("mcp:{name}")));
        {
            let mut s = ST.lock();
            s.ctx.insert(name.clone(), ctx.clone());
            s.sessions.insert(name.clone(), session.clone());
        }
        let (mut n, mut c) = (0, 0);
        for tl in &tools {
            let tname = tl["name"].as_str().unwrap_or("").to_string();
            if deny.iter().any(|w| tname.to_lowercase().contains(w)) {
                continue;
            }
            let full: String = format!("{name}__{tname}").chars().take(64).collect();
            let (ui, vis) = mcpapps::tool_ui(&tl["_meta"]);
            let params = tl.get("inputSchema").filter(|v| v.is_object()).cloned().unwrap_or(json!({"type": "object", "properties": {}}));
            let desc = tl["description"].as_str().unwrap_or("");
            let mut tool = Tool::new(
                &full,
                json!({"type": "function", "function": {"name": full, "description": py::head(desc, 1000), "parameters": params}}),
                make_run(name.clone(), tname.clone(), full.clone(), ui.clone(), session.clone(), stop.clone()),
            );
            tool.where_ = if vis.iter().any(|v| v == "model") { vec![WORKER, APPLET] } else { vec![] };
            tool.gated = true;
            tool.group = name.clone();
            tool.parallel = parallel_of(&name, &a, tl, ui.is_some());
            tool.meta = json!({"mcp": {"tool": tname, "ui": ui, "visibility": vis, "description": desc, "inputSchema": params}});
            if ctx.tool(tool).is_err() {
                continue;
            }
            if needs_confirm_tool(&a, tl) {
                ST.lock().confirm.insert(full.clone());
                c += 1;
            }
            n += 1;
        }
        let l = Later::new("已连接，{n} 个工具（{c} 个写操作默认要确认）", &[("n", json!(n)), ("c", json!(c))]);
        {
            let mut s = ST.lock();
            s.status.insert(name.clone(), l.clone());
            s.connected.insert(name.clone());
            s.needs_login.remove(&name);
        }
        connected = Some(l);
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(()));
        }
        changed();
        let _ = stop.wait_for(|v| *v).await;
        session.shutdown().await;
        Ok(())
    }
    .await;
    if let Err((msg, login)) = r {
        if login {
            mark_needs_login(&name, true);
        }
        let text = msg.text();
        ST.lock().status.insert(name.clone(), msg);
        if let Some(r) = ready.take() {
            let _ = r.send(Err(text));
        }
    }
    drop_tools(&name);
    {
        let mut s = ST.lock();
        s.connected.remove(&name);
        s.sessions.remove(&name);
        if s.running.get(&name).is_some_and(|r| r.generation == generation) {
            s.running.remove(&name);
        }
        if let Some(c) = &connected {
            if s.status.get(&name) == Some(c) {
                s.status.insert(name.clone(), Later::plain("已停用"));
            }
        }
    }
    mcpapps::forget(&name);
    let _ = done.send(true);
    changed();
}

pub fn connect(name: &str, timeout: f64) -> BoxFuture<'static, Result<String, String>> {
    let name = name.to_string();
    Box::pin(async move {
        let a = get(&name).ok_or_else(|| t!("没有应用 {name}", name = name))?;
        disconnect(&name, true).await;
        let interactive = mcp_oauth::interactive(&name);
        if py::truthy(&a["oauth"]) && !interactive && !mcp_oauth::has_tokens(&name) {
            mark_needs_login(&name, true);
            set_status(&name, Later::plain("还没有登录授权"));
            changed();
            return Ok(status_text(&name));
        }
        set_status(&name, Later::plain("正在连接…"));
        changed();
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let generation = GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ST.lock().running.insert(name.clone(), Running { stop: stop_tx, done: done_rx, generation });
        tokio::spawn(serve(name.clone(), a, rtx, stop_rx, if interactive { timeout - 10.0 } else { 120.0 }, done_tx, generation));
        match tokio::time::timeout(Duration::from_secs_f64(timeout), rrx).await {
            Err(_) => {
                set_status(&name, Later::plain("连接超时"));
                disconnect(&name, false).await;
            }
            Ok(_) => {}
        }
        Ok(status_text(&name))
    })
}

pub async fn disconnect(name: &str, keep_login: bool) {
    if !keep_login {
        mcp_oauth::cancel_login(name);
    }
    let item = ST.lock().running.remove(name);
    let Some(item) = item else { return };
    let _ = item.stop.send(true);
    let mut done = item.done.clone();
    let _ = tokio::time::timeout(Duration::from_secs(10), done.wait_for(|v| *v)).await;
    drop_tools(name);
    ST.lock().connected.remove(name);
}

pub async fn start() {
    seed();
    for (name, a) in all_apps_map() {
        if a["enabled"] == true {
            tokio::spawn(async move {
                let _ = connect(&name, 150.0).await;
            });
        } else {
            set_status(&name, Later::plain("未启用"));
        }
    }
}

pub async fn stop() {
    let names: Vec<String> = ST.lock().running.keys().cloned().collect();
    for n in names {
        disconnect(&n, false).await;
    }
}
