//! 事件：某件事发生时（收到 webhook、订阅源有新文章、网页变了……），Bot 在后台处理一次。对应 osa/events.py（说明见那里）。
//!
//! 事件和定时任务存在同一张表（routines），运行、运行记录、“无新内容不打扰”都共用 routines.rs；这里管事件特有的部分。

use crate::db::{self, RowExt};
use crate::registry::{self, HookReq, POLL, PUSH, ParseErr, Parsed, Source};
use crate::{i18n, py, routines, util, vault};
use parking_lot::Mutex;
use regex::Regex;
use rusqlite::params;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

pub const MAX_EVENTS: usize = 30;
pub const MAX_TEXT: usize = 20_000;
pub const MAX_PENDING: usize = 50;
pub const MAX_BODY: usize = 1_000_000;
pub const POLL_TIMEOUT: u64 = 60;
pub const POLL_PARALLEL: usize = 4;
pub const CATEGORIES: [&str; 6] = ["开发", "资讯", "生活", "金融", "系统", "通用"];

pub fn migrate() {
    db::batch(
        "create table if not exists event_state(
          routine_id integer primary key, state text, checked real, error text default '', fails integer default 0,
          sample text default '')",
    );
}

/// routine_id -> 排队的事件
static PENDING: LazyLock<Mutex<HashMap<i64, Vec<Value>>>> = LazyLock::new(Default::default);
/// 任务 id -> 派它的事件（internal 来源不让一个事件被自己派的任务再触发）。按插入顺序，最多 500 个
pub static TASK_ROUTINE: LazyLock<Mutex<indexmap::IndexMap<i64, i64>>> = LazyLock::new(Default::default);
static POLLING: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(Default::default);
static SEM: LazyLock<Arc<tokio::sync::Semaphore>> = LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(POLL_PARALLEL)));

// ---------- 来源 ----------
pub fn get_source(sid: &str) -> Option<Arc<Source>> {
    registry::source(sid)
}

pub fn unavailable(src: &Source) -> String {
    src.available.map(|f| f()).unwrap_or_default()
}

fn tr(v: &Value) -> String {
    v.as_str().map(|s| t!(s)).unwrap_or_default()
}

fn field_public(f: &Value) -> Value {
    let mut out = Map::new();
    for (k, v) in f.as_object().cloned().unwrap_or_default() {
        if !["label", "help", "placeholder", "options"].contains(&k.as_str()) {
            out.insert(k, v);
        }
    }
    let label = f["label"].as_str().filter(|s| !s.is_empty()).or(f["key"].as_str()).unwrap_or("");
    out.insert("label".into(), json!(t!(label)));
    for k in ["help", "placeholder"] {
        if py::truthy(&f[k]) {
            out.insert(k.into(), json!(tr(&f[k])));
        }
    }
    if let Some(opts) = f["options"].as_array().filter(|o| !o.is_empty()) {
        out.insert(
            "options".into(),
            json!(opts.iter().map(|o| {
                let lb = o["label"].as_str().filter(|s| !s.is_empty()).or(o["value"].as_str()).unwrap_or("");
                json!({"value": o["value"], "label": t!(lb)})
            }).collect::<Vec<_>>()),
        );
    }
    Value::Object(out)
}

fn scenario_public(src: &Source, sc: &Value) -> Value {
    let name = if py::truthy(&sc["name"]) { tr(&sc["name"]) } else { tr(&sc["title"]) };
    let mut out = json!({"source": src.id, "title": tr(&sc["title"]), "description": tr(&sc["description"]),
        "name": name, "params": if py::truthy(&sc["params"]) { sc["params"].clone() } else { json!({}) },
        "prompt": tr(&sc["prompt"])});
    for k in ["include", "exclude"] {
        out[k] = if py::truthy(&sc[k]) { sc[k].clone() } else { json!([]) };
    }
    out["llm_filter"] = json!(if py::truthy(&sc["llm_filter"]) { tr(&sc["llm_filter"]) } else { String::new() });
    if py::truthy(&sc["every"]) {
        out["every"] = sc["every"].clone();
    }
    out
}

/// 网页上“新建事件”用：所有来源（按分类）和它们的常见用法
pub fn catalog() -> Vec<Value> {
    let mut out: Vec<Value> = registry::sources()
        .iter()
        .map(|src| {
            let m = &src.meta;
            json!({"id": src.id, "title": tr(&m["title"]), "description": tr(&m["description"]), "mode": src.mode,
                "category": tr(&m["category"]), "icon": m["icon"], "fields": src.fields().iter().map(field_public).collect::<Vec<_>>(),
                "every": m["every"], "min_every": m["min_every"], "setup": if py::truthy(&m["setup"]) { tr(&m["setup"]) } else { String::new() },
                "unavailable": unavailable(src),
                "scenarios": m["scenarios"].as_array().map(|a| a.iter().map(|sc| scenario_public(src, sc)).collect::<Vec<_>>()).unwrap_or_default()})
        })
        .collect();
    let order = |cat: &str| CATEGORIES.iter().position(|c| t!(c) == cat).unwrap_or(99);
    out.sort_by_key(|s| order(s["category"].as_str().unwrap_or("")));
    out
}

/// 给模型看的来源清单
pub fn brief_sources() -> String {
    let mut lines = vec![];
    for src in registry::sources() {
        if !unavailable(&src).is_empty() {
            continue;
        }
        let fs: Vec<String> = src
            .fields()
            .iter()
            .map(|f| {
                let opts = f["options"].as_array().filter(|o| !o.is_empty()).map(|o| format!("，可选：{}", o.iter().map(|x| py::str_of(&x["value"])).collect::<Vec<_>>().join("/"))).unwrap_or_default();
                format!("{}（{}{}{opts}）", py::str_of(&f["key"]), f["label"].as_str().unwrap_or(""), if py::truthy(&f["required"]) { "，必填" } else { "" })
            })
            .collect();
        let fs = fs.join("、");
        lines.push(format!("- {}：{}。{}{}", src.id, src.title(), src.meta["description"].as_str().unwrap_or(""), if fs.is_empty() { String::new() } else { format!(" 参数：{fs}") }));
    }
    lines.join("\n")
}

// ---------- 参数 ----------
pub fn split_words(v: &Value) -> Vec<String> {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\n,，]+").unwrap());
    let items: Vec<String> = match v {
        Value::String(s) => R.split(s).map(String::from).collect(),
        Value::Array(a) => a.iter().map(py::str_of).collect(),
        Value::Null => vec![],
        other if !py::truthy(other) => vec![],
        other => vec![py::str_of(other)],
    };
    items.into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn secret(v: &Value, old: &Value) -> Result<Value, String> {
    let s = if py::truthy(v) { py::str_of(v).trim().to_string() } else { String::new() };
    if s.is_empty() {
        return Ok(if py::truthy(old) { old.clone() } else { json!("") });
    }
    if s.starts_with("vault:") {
        return Ok(json!(s));
    }
    let name = format!("event_{}", util::token_hex(6));
    vault::put(&name, &s)?;
    Ok(json!(format!("vault:{name}")))
}

fn num_str(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 1e15 { json!(v as i64) } else { json!(v) }
}

pub fn clean_params(src: &Source, raw: &Value, old: Option<&Value>) -> Result<Value, String> {
    let empty = json!({});
    let old = old.filter(|o| o.is_object()).unwrap_or(&empty);
    let raw = if raw.is_object() { raw.clone() } else { json!({}) };
    let mut out = Map::new();
    for f in src.fields() {
        let k = f["key"].as_str().unwrap_or("").to_string();
        let typ = f["type"].as_str().unwrap_or("text");
        let label = t!(f["label"].as_str().filter(|s| !s.is_empty()).unwrap_or(&k));
        let mut v = if raw.get(&k).is_some() { raw[&k].clone() } else if old.get(&k).is_some() { old[&k].clone() } else { f.get("default").cloned().unwrap_or(Value::Null) };
        match typ {
            "secret" => v = secret(&v, &old[&k])?,
            "text" | "textarea" | "url" => {
                let s = if v.is_null() { String::new() } else { py::str_of(&v) };
                let s = py::head(s.trim(), if typ == "textarea" { 4000 } else { 1000 });
                static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://[^\s/]+").unwrap());
                if typ == "url" && !s.is_empty() && !URL.is_match(&s) {
                    return Err(t!("{label}要以 http:// 或 https:// 开头", label = label));
                }
                v = json!(s);
            }
            "number" => {
                if v.is_null() || v == json!("") {
                    v = Value::Null;
                } else {
                    let n = match &v {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.trim().parse::<f64>().ok(),
                        Value::Bool(b) => Some(*b as i64 as f64),
                        _ => None,
                    }
                    .ok_or_else(|| t!("{label}要填数字", label = label))?;
                    if let Some(mn) = f["min"].as_f64() {
                        if n < mn {
                            return Err(t!("{label}至少是 {n}", label = label, n = f["min"].clone()));
                        }
                    }
                    if let Some(mx) = f["max"].as_f64() {
                        if n > mx {
                            return Err(t!("{label}最多是 {n}", label = label, n = f["max"].clone()));
                        }
                    }
                    v = num_str(n);
                }
            }
            "bool" => {
                v = json!(!matches!(&v, Value::Bool(false) | Value::Null) && v != json!(0) && v != json!("0") && v != json!("false") && v != json!("False") && v != json!(""));
            }
            "select" => {
                let s = if py::truthy(&v) { py::str_of(&v) } else { String::new() };
                let opts: Vec<String> = f["options"].as_array().map(|o| o.iter().map(|x| py::str_of(&x["value"])).collect()).unwrap_or_default();
                if !s.is_empty() && !opts.contains(&s) {
                    return Err(t!("{label}只能选：{options}", label = label, options = opts.join(" / ")));
                }
                v = json!(s);
            }
            "multi" => {
                let allowed: Vec<String> = f["options"].as_array().map(|o| o.iter().map(|x| py::str_of(&x["value"])).collect()).unwrap_or_default();
                v = json!(split_words(&v).into_iter().filter(|x| allowed.contains(x)).collect::<Vec<_>>());
            }
            "list" => {
                let mut l = split_words(&v);
                l.truncate(100);
                v = json!(l);
            }
            _ => {}
        }
        if py::truthy(&f["required"]) && (v.is_null() || v == json!("") || v == json!([])) {
            return Err(t!("要填{label}", label = label));
        }
        out.insert(k, v);
    }
    let out = Value::Object(out);
    match src.validate {
        Some(f) => f(out),
        None => Ok(out),
    }
}

/// 给来源用的参数：secret 换成保险库里的值
pub fn resolve(src: &Source, params: &Value) -> Value {
    let mut out = if params.is_object() { params.clone() } else { json!({}) };
    for f in src.fields() {
        if f["type"] == "secret" {
            let k = f["key"].as_str().unwrap_or("");
            if let Some(v) = out[k].as_str().filter(|v| v.starts_with("vault:")).map(String::from) {
                out[k] = json!(vault::get(&v[6..]).unwrap_or_default());
            }
        }
    }
    out
}

/// 检查事件的 trigger。old 是原来的 trigger
pub fn clean(t: &Value, old: &Value) -> Result<Value, String> {
    let empty = json!({});
    let old = if old.is_object() && old["kind"] == "event" { old } else { &empty };
    let sid = if py::truthy(&t["source"]) { py::str_of(&t["source"]) } else if py::truthy(&old["source"]) { py::str_of(&old["source"]) } else { String::new() };
    let src = get_source(&sid).ok_or_else(|| t!("没有这种事件来源：{source}", source = if sid.is_empty() { "—".to_string() } else { sid.clone() }))?;
    let why = unavailable(&src);
    if !why.is_empty() {
        return Err(why);
    }
    let same = old["source"].as_str() == Some(sid.as_str());
    let raw = if t["params"].is_object() { t["params"].clone() } else { json!({}) };
    let params = clean_params(&src, &raw, if same { Some(&old["params"]) } else { None })?;
    let words = |k: &str| -> Value {
        let v = if t.get(k).is_some() { &t[k] } else { &old[k] };
        let mut w = split_words(v);
        w.truncate(50);
        json!(w)
    };
    let lf = if t.get("llm_filter").is_some() { &t["llm_filter"] } else { &old["llm_filter"] };
    let lf = if py::truthy(lf) { py::str_of(lf) } else { String::new() };
    let mut out = json!({"kind": "event", "source": sid, "params": params, "include": words("include"), "exclude": words("exclude"),
                         "llm_filter": py::head(lf.trim(), 1000)});
    if src.mode == POLL {
        let pick = if py::truthy(&t["every"]) { t["every"].clone() } else if same && py::truthy(&old["every"]) { old["every"].clone() } else { json!(src.every()) };
        let every = py::to_int(&pick).unwrap_or(src.every());
        if every < src.min_every() {
            return Err(t!("这种事件最快 {n} 分钟看一次", n = src.min_every()));
        }
        out["every"] = json!(every.min(7 * 24 * 60));
    }
    if src.mode == PUSH {
        out["key"] = if py::truthy(&old["key"]) { old["key"].clone() } else { json!(new_key()) };
        if py::truthy(&old["path"]) {
            out["path"] = old["path"].clone();
        }
    }
    Ok(out)
}

pub fn new_key() -> String {
    util::token_urlsafe(18)
}

/// 导出成模板时的 trigger：去掉密钥、旧地址和 secret 参数
pub fn portable(t: &Value) -> Value {
    if t["kind"] != "event" {
        return t.clone();
    }
    let secret: Vec<String> = get_source(t["source"].as_str().unwrap_or("")).map(|s| s.fields().iter().filter(|f| f["type"] == "secret").map(|f| py::str_of(&f["key"])).collect()).unwrap_or_default();
    let mut out = Map::new();
    for (k, v) in t.as_object().cloned().unwrap_or_default() {
        if k != "key" && k != "path" {
            out.insert(k, v);
        }
    }
    let params: Map<String, Value> = t["params"].as_object().cloned().unwrap_or_default().into_iter().filter(|(k, _)| !secret.contains(k)).collect();
    out.insert("params".into(), Value::Object(params));
    Value::Object(out)
}

/// 旧版 kind=webhook → source=webhook 的事件，旧地址 /hooks/<名字> 记在 path 里
pub fn migrate_legacy(t: &Value, name: &str) -> Value {
    let or = |v: &Value, d: Value| if py::truthy(v) { v.clone() } else { d };
    let mut out = json!({"kind": "event", "source": "webhook", "params": {}, "include": or(&t["include"], json!([])),
        "exclude": or(&t["exclude"], json!([])), "llm_filter": or(&t["llm_filter"], json!("")), "key": new_key()});
    static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w-]{1,40}$").unwrap());
    if NAME.is_match(name) {
        out["path"] = json!(name);
    }
    out
}

pub fn reset(rid: i64) {
    db::exec("delete from event_state where routine_id=?", params![rid]);
}

/// 删事件时：状态、排队的事件、自动存进保险库的 secret 一起删
pub fn forget(r: &Value) {
    let rid = r["id"].as_i64().unwrap_or(0);
    db::exec("delete from event_state where routine_id=?", params![rid]);
    PENDING.lock().remove(&rid);
    for v in r["trigger"]["params"].as_object().cloned().unwrap_or_default().values() {
        if let Some(s) = v.as_str().filter(|s| s.starts_with("vault:event_")) {
            vault::delete(&s[6..]);
        }
    }
}

// ---------- 说明 ----------
pub fn describe(t: &Value, lang: Option<&str>) -> String {
    let sid = t["source"].as_str().unwrap_or("");
    let Some(src) = get_source(sid) else {
        return i18n::t_in("事件来源 {source} 现在没有了", lang, &[("source", json!(sid))]);
    };
    let title = i18n::t_in(src.title(), lang, &[]);
    let empty = json!({});
    let what = src.summary.map(|f| f(if t["params"].is_object() { &t["params"] } else { &empty })).unwrap_or_default();
    let mut s = if what.is_empty() { title } else { format!("{title}：{what}") };
    if src.mode == POLL && py::truthy(&t["every"]) {
        let m = py::to_int(&t["every"]).unwrap_or(0);
        s += &if m % 60 == 0 {
            i18n::t_in("（每 {n} 小时看一次）", lang, &[("n", json!(m / 60))])
        } else {
            i18n::t_in("（每 {n} 分钟看一次）", lang, &[("n", json!(m))])
        };
    }
    s
}

pub fn hook_path(t: &Value) -> String {
    if t["kind"] == "event" && py::truthy(&t["key"]) { format!("/hooks/{}", py::str_of(&t["key"])) } else { String::new() }
}

/// 网页的地址（channels.public_url）
pub fn public_url() -> String {
    crate::channels::public_url()
}

pub fn hook_url(t: &Value) -> String {
    let p = hook_path(t);
    if p.is_empty() { p } else { public_url() + &p }
}

pub fn state_of(rid: i64) -> Value {
    let mut out = match db::row("select checked, error, fails, sample from event_state where routine_id=?", params![rid]) {
        Some(r) => json!({"checked": r[0], "error": r.s(1), "fails": r.oi(2).unwrap_or(0), "has_sample": !r.s(3).is_empty()}),
        None => json!({"checked": null, "error": "", "fails": 0, "has_sample": false}),
    };
    out["pending"] = json!(PENDING.lock().get(&rid).map(|q| q.len()).unwrap_or(0));
    out
}

fn load_state(rid: i64) -> Value {
    db::row("select state from event_state where routine_id=?", params![rid])
        .map(|r| r.s(0))
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

fn save_state(rid: i64, state: Option<&Value>, error: &str, sample: Option<&Value>, keep_state: bool) {
    let now = py::now();
    let old = db::row("select state, fails, sample from event_state where routine_id=?", params![rid]);
    let st: Option<String> = if keep_state && old.is_some() {
        old.as_ref().and_then(|o| o[0].as_str().map(String::from))
    } else {
        state.map(py::dumps_u)
    };
    let fails = if !error.is_empty() && old.is_some() { old.as_ref().map(|o| o.i(1)).unwrap_or(0) + 1 } else if !error.is_empty() { 1 } else { 0 };
    let smp = match sample.filter(|s| py::truthy(s)) {
        Some(s) => py::dumps_u(s),
        None => old.as_ref().map(|o| o.s(2)).unwrap_or_default(),
    };
    db::exec(
        "insert or replace into event_state(routine_id, state, checked, error, fails, sample) values(?,?,?,?,?,?)",
        params![rid, st, now, py::head(error, 300), fails, smp],
    );
}

// ---------- 把事件交给 Bot ----------
fn ev(e: &Value) -> Option<Value> {
    if !py::truthy(e) {
        return None;
    }
    if let Some(s) = e.as_str() {
        return Some(json!({"title": py::head(s, 120), "text": s}));
    }
    let mut e = e.clone();
    let title = if py::truthy(&e["title"]) { py::str_of(&e["title"]) } else { String::new() };
    let text = if py::truthy(&e["text"]) { py::str_of(&e["text"]) } else { String::new() };
    e["title"] = json!(py::head(&title, 300));
    e["text"] = json!(text);
    Some(e)
}

fn plain(e: &Value) -> String {
    ["title", "text", "url"].iter().map(|k| if py::truthy(&e[*k]) { py::str_of(&e[*k]) } else { String::new() }).collect::<Vec<_>>().join("\n")
}

pub fn render(src: Option<&Source>, params: &Value, events: &[Value]) -> String {
    let mut head = format!("事件来源：{}", src.map(|s| s.title()).unwrap_or("?"));
    if let Some(f) = src.and_then(|s| s.summary) {
        let what = f(params);
        if !what.is_empty() {
            head += &format!("（{what}）");
        }
    }
    let mut parts = vec![head + &if events.len() > 1 { format!("，共 {} 条", events.len()) } else { String::new() }];
    let mut used = 0;
    for (i, e) in events.iter().take(MAX_EVENTS).enumerate() {
        let i = i + 1;
        let title = e["title"].as_str().unwrap_or("").trim().to_string();
        let mut block = format!("{}{title}", if events.len() > 1 { format!("{i}. ") } else { String::new() });
        if py::truthy(&e["url"]) {
            block += &format!("\n链接：{}", py::str_of(&e["url"]));
        }
        let text = e["text"].as_str().unwrap_or("").trim().to_string();
        if !text.is_empty() && text != title {
            block += &format!("\n{text}");
        }
        let blen = py::char_len(&block);
        if used + blen > MAX_TEXT {
            parts.push(format!("……还有 {} 条，太长没放进来", events.len() - i + 1));
            break;
        }
        used += blen;
        parts.push(block);
    }
    if events.len() > MAX_EVENTS {
        parts.push(format!("……还有 {} 条，太多没放进来", events.len() - MAX_EVENTS));
    }
    parts.join("\n\n")
}

/// 事件来了：关键词逐条过滤 → 正在跑就排队 → LLM 过滤 → 派给 Bot。source：webhook / event / test
pub fn deliver(r: Value, events: Vec<Value>, source: &'static str) -> futures::future::BoxFuture<'static, Value> {
    Box::pin(async move {
        let rid = r["id"].as_i64().unwrap_or(0);
        let trig = r["trigger"].clone();
        let mut events: Vec<Value> = events.iter().filter_map(ev).collect();
        if events.is_empty() {
            return json!({"accepted": false, "reason": t!("没有事件")});
        }
        if source != "test" {
            let kept: Vec<Value> = events.iter().filter(|e| routines::keyword_filter(&trig, &plain(e)).is_none()).cloned().collect();
            if kept.is_empty() {
                let reason = routines::keyword_filter(&trig, &plain(&events[0])).unwrap_or_default();
                let n = if events.len() > 1 { format!("（{}）", events.len()) } else { String::new() };
                routines::record(rid, source, "filtered", &(reason.clone() + &n), None);
                routines::changed(&r);
                return json!({"accepted": false, "stage": "rule", "reason": reason});
            }
            events = kept;
        }
        if routines::is_running(rid) {
            let n = {
                let mut p = PENDING.lock();
                let q = p.entry(rid).or_default();
                q.extend(events);
                let len = q.len();
                if len > MAX_PENDING {
                    q.drain(..len - MAX_PENDING);
                }
                q.len()
            };
            routines::changed(&r);
            return json!({"accepted": true, "queued": n});
        }
        let src = get_source(trig["source"].as_str().unwrap_or(""));
        let empty = json!({});
        let text = render(src.as_deref(), if trig["params"].is_object() { &trig["params"] } else { &empty }, &events);
        let rule = trig["llm_filter"].as_str().unwrap_or("").to_string();
        if source != "test" && !rule.is_empty() {
            let (ok, reason) = match routines::llm_filter(&rule, &text, r["bot_id"].as_i64()).await {
                Ok(x) => x,
                Err(e) => (true, t!("过滤出错，默认放行：{error}", error = py::head(&e, 100))),
            };
            if !ok {
                routines::record(rid, source, "filtered", &t!("LLM：{reason}", reason = reason.clone()), None);
                routines::changed(&r);
                return json!({"accepted": false, "stage": "llm", "reason": reason});
            }
        }
        routines::fire(r, source, text).await
    })
}

/// 一次运行结束：排队的事件接着处理
pub async fn after_run(r: &Value) {
    let rid = r["id"].as_i64().unwrap_or(0);
    let q = PENDING.lock().remove(&rid);
    if let Some(q) = q.filter(|q| !q.is_empty()) {
        if let Some(fresh) = routines::get(rid).filter(|f| f["enabled"] == true) {
            deliver(fresh, q, "event").await;
        }
    }
}

// ---------- push ----------
pub fn by_key(key: &str) -> Option<Value> {
    routines::list_for(None).into_iter().find(|r| {
        let k = r["trigger"]["key"].as_str().unwrap_or("");
        r["trigger"]["kind"] == "event" && !k.is_empty() && util_eq(k.as_bytes(), key.as_bytes())
    })
}

fn util_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 旧地址 /hooks/<名字>
pub fn by_path(name: &str) -> Option<Value> {
    routines::list_for(None).into_iter().find(|r| r["trigger"]["kind"] == "event" && r["trigger"]["path"].as_str() == Some(name))
}

/// webhook 收到的请求。Err 是签名不对（接口回 401）
pub async fn on_push(r: Value, headers: HashMap<String, String>, body: Vec<u8>, query: HashMap<String, String>) -> Result<Value, String> {
    let data: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap_or(Value::Null) };
    if data.is_object() && data["type"] == "url_verification" && data.get("challenge").is_some() {
        return Ok(json!({"challenge": data["challenge"]}));
    }
    let rid = r["id"].as_i64().unwrap_or(0);
    if r["enabled"] != true {
        return Ok(json!({"accepted": false, "reason": t!("这个事件暂停了")}));
    }
    let sid = r["trigger"]["source"].as_str().unwrap_or("").to_string();
    let Some(src) = get_source(&sid).filter(|s| s.mode == PUSH) else {
        return Ok(json!({"accepted": false, "reason": t!("事件来源 {source} 现在没有了", source = sid)}));
    };
    let req = HookReq { headers: headers.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect(), body, json: data, query };
    let mut p = resolve(&src, &r["trigger"]["params"]);
    p["_key"] = json!(r["trigger"]["key"].as_str().unwrap_or(""));
    let parse = src.parse.expect("push source has parse");
    let out = match parse(&req, &p) {
        Ok(o) => o,
        Err(ParseErr::Denied(e)) => {
            routines::record(rid, "webhook", "failed", &e, None);
            routines::changed(&r);
            return Err(e);
        }
        Err(ParseErr::Other(e)) => {
            routines::record(rid, "webhook", "failed", &t!("解析出错：{error}", error = py::head(&e, 200)), None);
            routines::changed(&r);
            return Ok(json!({"accepted": false, "reason": e}));
        }
    };
    let events = match out {
        Parsed::Skip(why) => {
            routines::record(rid, "webhook", "ignored", &why, None);
            routines::changed(&r);
            return Ok(json!({"accepted": false, "reason": why}));
        }
        Parsed::Events(e) => e,
    };
    if let Some(first) = events.first().and_then(ev) {
        save_state(rid, None, "", Some(&first), true);
    }
    Ok(deliver(r, events, "webhook").await)
}

// ---------- poll ----------
struct Polling(i64);
impl Drop for Polling {
    fn drop(&mut self) {
        POLLING.lock().remove(&self.0);
    }
}

/// 看一次这个事件的来源，有新的就交给 Bot
pub fn poll(r: Value) -> futures::future::BoxFuture<'static, Value> {
    Box::pin(async move {
        let rid = r["id"].as_i64().unwrap_or(0);
        let src = get_source(r["trigger"]["source"].as_str().unwrap_or(""));
        let Some(src) = src.filter(|s| s.mode == POLL) else {
            return json!({"accepted": false, "reason": t!("正在检查")});
        };
        if !POLLING.lock().insert(rid) {
            return json!({"accepted": false, "reason": t!("正在检查")});
        }
        let _guard = Polling(rid);
        let (events, first) = {
            let _permit = SEM.clone().acquire_owned().await;
            let state = load_state(rid);
            let first = state.is_null();
            let check = src.check.expect("poll source has check");
            let params = resolve(&src, &r["trigger"]["params"]);
            let got = tokio::time::timeout(Duration::from_secs(POLL_TIMEOUT), check(params, state.clone())).await;
            let (events, new_state) = match got {
                Ok(Ok(x)) => x,
                Ok(Err(msg)) => {
                    save_state(rid, None, &py::head(&msg, 300), None, true);
                    routines::changed(&r);
                    return json!({"accepted": false, "reason": msg});
                }
                Err(_) => {
                    let msg = t!("{n} 秒内没看完", n = POLL_TIMEOUT);
                    save_state(rid, None, &msg, None, true);
                    routines::changed(&r);
                    return json!({"accepted": false, "reason": msg});
                }
            };
            let events: Vec<Value> = events.iter().filter_map(ev).collect();
            let st = if new_state.is_null() { if state.is_null() { json!({}) } else { state } } else { new_state };
            save_state(rid, Some(&st), "", events.first(), false);
            (events, first)
        };
        if events.is_empty() {
            routines::changed(&r);
            return json!({"accepted": false, "reason": if first { t!("已记下现状，之后有新的才通知") } else { t!("没有新的") }});
        }
        if first && !src.fire_first() {
            routines::changed(&r);
            return json!({"accepted": false, "reason": t!("已记下现状，之后有新的才通知")});
        }
        deliver(r, events, "event").await
    })
}

pub fn due_poll(r: &Value) -> bool {
    let rid = r["id"].as_i64().unwrap_or(0);
    r["trigger"]["kind"] == "event"
        && get_source(r["trigger"]["source"].as_str().unwrap_or("")).is_some_and(|s| s.mode == POLL)
        && !POLLING.lock().contains(&rid)
}

// ---------- internal ----------
/// osa 自己的事件到了：交给所有用这个来源、开着的事件。不等它们跑完
pub fn internal(source_id: &str, kw: &Value) {
    let Some(src) = get_source(source_id) else { return };
    let Some(matcher) = src.matcher else { return };
    let task_id = kw["task"]["id"].as_i64();
    for r in routines::list_for(None) {
        let t = &r["trigger"];
        if r["enabled"] != true || t["kind"] != "event" || t["source"].as_str() != Some(source_id) {
            continue;
        }
        if let Some(tid) = task_id {
            if TASK_ROUTINE.lock().get(&tid).copied() == r["id"].as_i64() {
                continue;
            }
        }
        let evs = matcher(&resolve(&src, &t["params"]), &r, kw);
        if !evs.is_empty() {
            tokio::spawn(deliver(r.clone(), evs, "event"));
        }
    }
}

pub fn remember_task(task_id: i64, rid: i64) {
    let mut m = TASK_ROUTINE.lock();
    m.insert(task_id, rid);
    while m.len() > 500 {
        m.shift_remove_index(0);
    }
}

// ---------- 测试 ----------
pub async fn test(r: Value) -> Value {
    let sid = r["trigger"]["source"].as_str().unwrap_or("").to_string();
    let Some(src) = get_source(&sid) else {
        return json!({"accepted": false, "reason": t!("事件来源 {source} 现在没有了", source = sid)});
    };
    if src.mode == POLL {
        let out = poll(r.clone()).await;
        if out["accepted"] == true {
            return out;
        }
    }
    let rid = r["id"].as_i64().unwrap_or(0);
    let sample = db::row("select sample from event_state where routine_id=?", params![rid])
        .map(|x| x.s(0))
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(py::truthy)
        .or_else(|| Some(src.meta["sample"].clone()).filter(py::truthy))
        .unwrap_or(json!({"title": "（这是一次手动测试，没有真实事件）", "text": ""}));
    deliver(r, vec![sample], "test").await
}
