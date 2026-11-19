//! 演示一遍（Teach a task）：用户在共用电脑上做一遍，Bot 把录到的操作整理成技能草稿，用户审核后保存。对应 osa/teach.py。
//!
//! 录制靠电脑（第 2 步才迁过来）：开始、结束时对电脑的几步操作集中在下面的 pc_* 里，现在电脑还没有，开始录制会照实报错；
//! 状态、自动结束、整理操作、生成草稿都已经照 Python 版写好。

use crate::web::{ApiError, Body, Res, ok};
use crate::{i18n, providers, py, server};
use axum::extract::Path;
use axum::routing::post;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const MAX_S: i64 = 600;
pub const KEY: &str = "teach";
const DRAFT_PROMPT: &str = include_str!("prompts/teach/draft.txt");

#[derive(Clone)]
struct State {
    bot_id: i64,
    started: f64,
    note: String,
    file: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static TIMER: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

pub fn public() -> Value {
    match &*STATE.lock() {
        Some(s) => json!({"bot_id": s.bot_id, "started": s.started, "note": s.note, "max_s": MAX_S}),
        None => Value::Null,
    }
}

fn changed(extra: Value, _pc: Option<i64>) {
    // TODO（第 2 步）：pc 给了时先推 {"type": "computer", "computer": computer.public_state(pc)}
    let mut ev = json!({"type": "teach", "teach": public()});
    if let Some(o) = extra.as_object() {
        for (k, v) in o {
            ev[k] = v.clone();
        }
    }
    server::broadcast(ev);
}

// ---------- 电脑上的几步（第 2 步） ----------
async fn pc_begin(_path: &str) -> Result<(), String> {
    // Python 版：c.users / c.exclusive 有人在用就报“电脑正在被用着”；c.start()；c.claim_exclusive(KEY, "演示录制")；
    // c.browser("rec_start", as_human=True, file=path, max_s=MAX_S+30)；c.takeover()；本机电脑 c.show()
    Err(t!("电脑现在用不了（Rust 版还没有电脑）"))
}

async fn pc_end(_path: &str) -> Result<Vec<Value>, String> {
    // Python 版：r = c.browser("rec_stop", as_human=True, file=path)；finally c.handback()、c.release(KEY)
    Ok(vec![])
}

pub async fn start(bot_id: i64, note: &str) -> Result<Value, String> {
    if STATE.lock().is_some() {
        return Err(t!("已经在录制了，先结束上一段"));
    }
    let path = format!("/tmp/osa-rec-{}.jsonl", py::now() as i64);
    pc_begin(&path).await?;
    let started = py::now();
    *STATE.lock() = Some(State { bot_id, started, note: py::head(note.trim(), 500), file: path });
    *TIMER.lock() = Some(tokio::spawn(auto_stop(started)));
    changed(json!({}), Some(bot_id));
    Ok(public())
}

async fn auto_stop(started: f64) {
    tokio::time::sleep(std::time::Duration::from_secs(MAX_S as u64)).await;
    if STATE.lock().as_ref().is_some_and(|s| s.started == started) {
        match stop_inner(true).await {
            Ok(r) => changed(json!({"bot_id": r["bot_id"], "draft": r["draft"], "steps": r["steps"], "auto": true}), None),
            Err(e) => changed(json!({"error": t!("自动结束录制时出错：{error}", error = e)}), None),
        }
    }
}

/// 停掉录制、交还电脑，返回 (录制信息, 录到的操作)
async fn finish(from_timer: bool) -> Result<(State, Vec<Value>), String> {
    let s = STATE.lock().take().ok_or_else(|| t!("现在没有在录制"))?;
    let timer = TIMER.lock().take();
    if let Some(t) = timer {
        if !from_timer {
            t.abort();
        }
    }
    let events = pc_end(&s.file).await;
    changed(json!({}), Some(s.bot_id));
    Ok((s, events?))
}

pub async fn cancel() -> Result<(), String> {
    finish(false).await.map(|_| ())
}

pub async fn stop() -> Result<Value, String> {
    stop_inner(false).await
}

async fn stop_inner(from_timer: bool) -> Result<Value, String> {
    let (s, events) = finish(from_timer).await?;
    let steps = describe(&events, s.started);
    if steps.is_empty() {
        return Err(t!("没录到浏览器里的操作。录制只记网页上的点击、输入和打开的网址"));
    }
    let draft = draft_skill(&steps, &s.note).await?;
    Ok(json!({"bot_id": s.bot_id, "draft": draft, "steps": steps}))
}

// ---------- 整理 ----------
fn netloc(url: &str) -> String {
    // urlparse(url).netloc：有 // 才有
    match url.split_once("//") {
        Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or("").to_string(),
        None => String::new(),
    }
}

fn s(e: &Value, k: &str) -> String {
    match e.get(k) {
        None | Some(Value::Null) => String::new(),
        Some(v) => py::str_of(v),
    }
}

/// e.get(k) or 默认值
fn or(e: &Value, k: &str, d: String) -> String {
    if py::truthy(&e[k]) { py::str_of(&e[k]) } else { d }
}

/// 把录到的事件排成一行一步的文字；合并重复的输入，去掉连续的同一网址
pub fn describe(events: &[Value], started: f64) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut last_url = String::new();
    let mut last: Option<(String, Value, String)> = None;
    for e in events {
        let kind = s(e, "type");
        let mut sig_m = std::collections::BTreeMap::new();
        if let Some(o) = e.as_object() {
            for (k, v) in o {
                if k != "t" && k != "title" && k != "url" {
                    sig_m.insert(k.clone(), v.clone());
                }
            }
        }
        // json.dumps(sort_keys=True, ensure_ascii=False)
        let sig = py::dumps_u(&sort_keys(&json!(sig_m)));
        if kind == "input" && last.as_ref().is_some_and(|l| l.0 == "input" && l.1 == e["field"]) {
            out.pop();
        } else if last.as_ref().map(|l| l.2.as_str()) == Some(sig.as_str()) && kind != "navigate" {
            continue;
        }
        let t = if py::truthy(&e["t"]) { e["t"].as_f64().unwrap_or(started) } else { started };
        let secs = (t - started).max(0.0) as i64;
        let when = format!("{:02}:{:02}", (secs / 60) % 60, secs % 60);
        let mut line;
        if kind == "navigate" {
            let url = s(e, "url");
            if url == last_url {
                continue;
            }
            last_url = url.clone();
            line = if py::truthy(&e["start"]) {
                t!("开始时在 {url}", url = url)
            } else if py::truthy(&e["new_tab"]) {
                t!("新标签页打开 {url}", url = url)
            } else {
                t!("页面到了 {url}", url = url)
            };
            if py::truthy(&e["title"]) {
                line += &t!("（{title}）", title = e["title"].clone());
            }
        } else if kind == "click" {
            let k = if e.get("kind").is_some() { s(e, "kind") } else { t!("元素") };
            line = t!("点击 {kind}「{name}」", kind = k, name = or(e, "name", t!("（无文字）")));
            if py::truthy(&e["href"]) {
                line += &format!(" → {}", s(e, "href"));
            }
        } else if kind == "input" {
            let v = if e.get("value").is_some() { s(e, "value") } else { String::new() };
            line = t!("在「{field}」输入「{value}」", field = or(e, "field", t!("输入框")), value = v);
            if py::truthy(&e["secret"]) {
                line += &t!("（密码类，没有记录内容）");
            }
            if py::truthy(&e["submit"]) {
                line += &t!("，然后按回车");
            }
        } else if kind == "select" {
            let v = if e.get("value").is_some() { s(e, "value") } else { String::new() };
            line = t!("在「{field}」选择「{value}」", field = or(e, "field", t!("下拉框")), value = v);
        } else if kind == "check" {
            let f = if e.get("field").is_some() { s(e, "field") } else { String::new() };
            line = if py::truthy(&e["checked"]) { t!("勾选「{field}」", field = f) } else { t!("取消勾选「{field}」", field = f) };
        } else if kind == "key" {
            line = i18n::t("按 {key}", &[("key", e.get("key").cloned().unwrap_or(Value::Null))]);
        } else {
            continue;
        }
        if kind != "navigate" && py::truthy(&e["url"]) && netloc(&s(e, "url")) != netloc(&last_url) {
            line += &t!("（在 {site}）", site = netloc(&s(e, "url")));
        }
        out.push(format!("{when} {line}"));
        last = Some((kind, e["field"].clone(), sig));
    }
    let n = out.len();
    if n > 300 {
        out.drain(..n - 300);
    }
    out
}

fn sort_keys(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let mut m = serde_json::Map::new();
            for k in keys {
                m.insert(k.clone(), sort_keys(&o[k]));
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(sort_keys).collect()),
        x => x.clone(),
    }
}

/// 模型偶尔会在 JSON 外面包代码块或多说一句：先整段解析，不行就取最外层的 {...}
fn parse(text: &str) -> Value {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{[\s\S]*\}").unwrap());
    for s in [text.to_string(), RE.find(text).map(|m| m.as_str().to_string()).unwrap_or_default()] {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if v.is_object() {
                return v;
            }
        }
    }
    json!({})
}

pub async fn draft_skill(steps: &[String], note: &str) -> Result<Value, String> {
    let user = format!("{}录到的操作：\n{}", if note.is_empty() { String::new() } else { format!("用户说要演示的是：{note}\n\n") }, steps.join("\n"));
    let msgs = json!([{"role": "system", "content": format!("{DRAFT_PROMPT}{}", i18n::model_hint())}, {"role": "user", "content": user}]);
    let mut d = json!({});
    for _ in 0..2 {
        let r = providers::complete_role("router", &msgs, Some(json!({"type": "json_object"}))).await.map_err(|e| providers::brief(&e))?;
        d = parse(&r.content);
        if py::truthy(&d["body"]) {
            break;
        }
    }
    static BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w一-鿿-]").unwrap());
    static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w一-鿿-]{1,40}$").unwrap());
    let raw = if py::truthy(&d["name"]) { py::str_of(&d["name"]) } else { String::new() };
    let mut name = py::head(&BAD.replace_all(&raw, ""), 40);
    if name.is_empty() || !NAME_RE.is_match(&name) {
        name = t!("演示{time}", time = py::strftime("%m%d%H%M"));
    }
    let body = if py::truthy(&d["body"]) { py::str_of(&d["body"]).trim().to_string() } else { String::new() };
    let body = if body.is_empty() { format!("{}\n{}", t!("按下面录到的步骤做："), steps.join("\n")) } else { body };
    let desc = if py::truthy(&d["description"]) { py::str_of(&d["description"]) } else { note.to_string() };
    Ok(json!({"name": name, "description": py::head(&desc, 200), "body": py::head(&body, 8000)}))
}

// ---------- 接口 ----------
async fn teach_start(Path(bot_id): Path<i64>, Body(d): Body) -> Res {
    crate::server::bot_or_404(bot_id)?;
    let note = d["note"].as_str().unwrap_or("").to_string();
    ok(start(bot_id, &note).await?)
}

async fn teach_action(Path(action): Path<String>) -> Res {
    match action.as_str() {
        "stop" => ok(stop().await?),
        "cancel" => {
            cancel().await?;
            ok(json!({"ok": true}))
        }
        _ => Err(ApiError::not_found(t!("未知操作"))),
    }
}

pub fn routes() -> axum::Router {
    axum::Router::new().route("/api/bots/{bot_id}/teach/start", post(teach_start)).route("/api/teach/{action}", post(teach_action))
}
