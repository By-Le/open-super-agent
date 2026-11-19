//! Bot 模板：把一个 Bot 的人设、头像、技能、要用的应用、定时任务打包成一个文件（.osabot.yaml），一键装出一个现成的助手。
//! 对应 osa/templates.py（格式说明见那里）。内置模板编进二进制（assets/skills.json）。
//!
//! 定时任务那几处要用 routines 模块（检查 trigger、建、删、导出时去掉密钥），在下面“定时任务”一节统一调用；
//! routines 还没迁过来的部分先在那里绕开。

use crate::db::{self, RowExt};
use crate::server::broadcast;
use crate::skills::{self, Apps};
use crate::web::{ApiError, Body, Res, ok};
use crate::{bots, catalog, i18n, mcp_hub, py, routines, util};
use axum::extract::Path;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use regex::Regex;
use rusqlite::params;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub const FORMAT: i64 = 1;
pub const MAX_SKILLS: usize = 30;
pub const MAX_ROUTINES: usize = 20;
pub const MAX_APPS: usize = 20;
const EN_KEYS: [&str; 4] = ["name", "description", "instructions", "apps"];
static APP_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]{0,23}$").unwrap());
static COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9a-fA-F]{6}$").unwrap());

pub fn migrate() {
    db::add_column("bots", "template_token", "text");
    db::batch("create unique index if not exists bots_template_token on bots(template_token)");
}

// ---------- 定时任务（routines 模块） ----------
/// routines._clean_trigger
fn clean_trigger(t: &Value) -> Result<Value, String> {
    // TODO(合并时)：换成 routines::clean_trigger(t, None)。routines 迁好之前只检查是一组键值
    if !t.is_object() {
        return Err("trigger 要是一组键值".into());
    }
    Ok(t.clone())
}

/// routines.save(bot_id, data)
fn save_routine(_bot_id: i64, _data: &Value) -> Result<(), String> {
    // TODO(合并时)：换成 routines::save(bot_id, data, None)。routines 迁好之前模板里的定时任务不装
    tracing::warn!("Rust 版的定时任务还没迁好，模板里的定时任务没有装");
    Ok(())
}

/// routines.delete_for_bot
fn delete_routines(_bot_id: i64) {
    // TODO(合并时)：换成 routines::delete_for_bot(bot_id)
}

/// events.portable：导出成模板时的 trigger 去掉密钥、旧地址（secret 参数也要去掉，要 events 的来源清单）
fn portable(t: &Value) -> Value {
    // TODO(合并时)：换成 events::portable(t)
    if t["kind"] != "event" {
        return t.clone();
    }
    let mut o = t.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("key");
        m.remove("path");
    }
    o
}

// ---------- 检查 ----------
fn yaml(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_yaml::from_str::<serde_yaml::Value>(text)
        .map(crate::config::yaml_to_json)
        .map_err(|e| t!("YAML 写错了：{err}", err = e.to_string().lines().next().unwrap_or("").to_string()))
}

fn list_fields(key: &str) -> (&'static [&'static str], &'static str) {
    if key == "skills" {
        (&["name", "description", "body"], "en.skills 要和 skills 一一对应：个数、顺序都一样，每一项只能写 name、description、body")
    } else {
        (&["name", "prompt"], "en.routines 要和 routines 一一对应：个数、顺序都一样，每一项只能写 name、prompt")
    }
}

fn check_en(t: &Value) -> Result<(), String> {
    let en = &t["en"];
    let ok_keys = en.as_object().is_some_and(|o| o.keys().all(|k| EN_KEYS.contains(&k.as_str()) || k == "skills" || k == "routines"));
    if !ok_keys {
        return Err(t!("en 里只能写 name、description、instructions、apps、skills、routines"));
    }
    for key in ["skills", "routines"] {
        let (fields, msg) = list_fields(key);
        let xs = &en[key];
        if xs.is_null() {
            continue;
        }
        let base = if t[key].is_null() { json!([]) } else { t[key].clone() };
        let good = match (xs.as_array(), base.as_array()) {
            (Some(x), Some(b)) => x.len() == b.len() && x.iter().all(|i| i.is_null() || i.as_object().is_some_and(|o| o.keys().all(|k| fields.contains(&k.as_str())))),
            _ => false,
        };
        if !good {
            return Err(t!(msg));
        }
    }
    Ok(())
}

fn over(base: &Value, e: &Value) -> Value {
    if !base.is_object() {
        return base.clone();
    }
    let mut o = base.clone();
    if let Some(m) = e.as_object() {
        for (k, v) in m {
            if py::truthy(v) {
                o[k] = v.clone();
            }
        }
    }
    o
}

fn english(t: &Value) -> Value {
    let en = &t["en"];
    let mut out = t.clone();
    for k in EN_KEYS {
        if py::truthy(&en[k]) {
            out[k] = en[k].clone();
        }
    }
    let mut rename: Map<String, Value> = Map::new();
    if let (Some(ss), Some(es)) = (t["skills"].as_array(), en["skills"].as_array().filter(|a| !a.is_empty())) {
        out["skills"] = json!(ss.iter().zip(es).map(|(s, e)| over(s, e)).collect::<Vec<_>>());
        for (s, e) in ss.iter().zip(es) {
            if s.is_object() && py::truthy(e) && py::truthy(&e["name"]) {
                rename.insert(skills::clean_name(&if py::truthy(&s["name"]) { py::str_of(&s["name"]) } else { String::new() }), json!(skills::clean_name(&py::str_of(&e["name"]))));
            }
        }
    }
    if let Some(rs) = t["routines"].as_array() {
        let ens: Vec<Value> = en["routines"].as_array().filter(|a| !a.is_empty()).cloned().unwrap_or_else(|| vec![Value::Null; rs.len()]);
        let merged: Vec<Value> = rs
            .iter()
            .zip(ens.iter())
            .map(|(r, e)| over(r, e))
            .map(|r| {
                if r.is_object() && py::truthy(&r["skill"]) {
                    let key = py::str_of(&r["skill"]).trim().trim_start_matches('/').to_string();
                    let mut r = r.clone();
                    if let Some(n) = rename.get(&key) {
                        r["skill"] = n.clone();
                    }
                    r
                } else {
                    r
                }
            })
            .collect();
        out["routines"] = json!(merged);
    }
    out
}

fn s_of(v: &Value) -> String {
    if py::truthy(v) { py::str_of(v) } else { String::new() }
}

/// 解析并检查一份模板，返回干净的字典
pub fn parse(text: &str, lang: Option<&str>) -> Result<Value, String> {
    if py::char_len(text) > 400_000 {
        return Err(t!("文件太大了，不像是一个 Bot 模板"));
    }
    let mut t = yaml(text)?;
    if !t.is_object() || t.get("osa_template").is_none() {
        return Err(t!("这不是 open-super-agent 的 Bot 模板（开头要有 osa_template: 1）"));
    }
    if py::as_int(&t["osa_template"]) != Some(FORMAT) || t["osa_template"].is_boolean() {
        return Err(t!("这个模板是更新版本的 open-super-agent 做的，先升级再装"));
    }
    if !t["en"].is_null() && t.get("en").is_some() {
        check_en(&t)?;
        let l = lang.map(String::from).unwrap_or_else(i18n::lang);
        if l == "en" {
            t = english(&t);
        }
    }
    let name = py::head(s_of(&t["name"]).trim(), 40);
    if name.is_empty() {
        return Err(t!("模板缺少 name（Bot 的名字）"));
    }
    let av = if t["avatar"].is_object() { t["avatar"].clone() } else { json!({}) };
    let shape = av["shape"].as_str().filter(|s| bots::SHAPES.contains(s)).unwrap_or(bots::SHAPES[0]);
    let color = av["color"].as_str().filter(|c| COLOR.is_match(c)).unwrap_or(bots::COLORS[0]);
    let avatar = json!({"shape": shape, "color": color});
    let instructions = s_of(&t["instructions"]).trim().to_string();
    if py::char_len(&instructions) > 4000 {
        return Err(t!("人设（instructions）最多 4000 个字"));
    }
    let apps = if py::truthy(&t["apps"]) { t["apps"].clone() } else { json!([]) };
    let apps_ok = apps.as_array().is_some_and(|a| a.len() <= MAX_APPS && a.iter().all(|x| x.as_str().is_some_and(|s| APP_ID.is_match(s))));
    if !apps_ok {
        return Err(t!("apps 应该是应用名的列表（小写字母开头，比如 notion、gmail）"));
    }
    let mut out_skills = vec![];
    let mut seen: Vec<String> = vec![];
    let raw_skills = if py::truthy(&t["skills"]) { t["skills"].clone() } else { json!([]) };
    let Some(raw_skills) = raw_skills.as_array().filter(|a| a.len() <= MAX_SKILLS) else {
        return Err(t!("skills 应该是一个列表，最多 {n} 个", n = MAX_SKILLS));
    };
    for (i, s) in raw_skills.iter().enumerate() {
        let i = i + 1;
        if !s.is_object() {
            return Err(t!("第 {i} 个技能应该写 name、description、body", i = i));
        }
        let sname = skills::clean_name(&s_of(&s["name"]));
        let body = s_of(&s["body"]).trim().to_string();
        if let Err(e) = skills::check(&sname, &body) {
            let label = if py::truthy(&s["name"]) { s["name"].clone() } else { json!(i) };
            return Err(t!("技能 {name}：{err}", name = label, err = e));
        }
        if seen.contains(&sname) {
            return Err(t!("技能 /{name} 写了两次", name = sname));
        }
        seen.push(sname.clone());
        let files = skills::check_files(&s["files"]).map_err(|e| t!("技能 {name}：{err}", name = sname, err = e))?;
        let desc = s_of(&s["description"]).split_whitespace().collect::<Vec<_>>().join(" ");
        out_skills.push(json!({"name": sname, "description": py::head(&desc, 200), "body": body, "files": files,
            "apps": if s.get("apps").is_some_and(|a| !a.is_null()) { json!(skills::clean_apps(&s["apps"])) } else { Value::Null }}));
    }
    let mut out_routines = vec![];
    let raw_routines = if py::truthy(&t["routines"]) { t["routines"].clone() } else { json!([]) };
    let Some(raw_routines) = raw_routines.as_array().filter(|a| a.len() <= MAX_ROUTINES) else {
        return Err(t!("routines 应该是一个列表，最多 {n} 个", n = MAX_ROUTINES));
    };
    for (i, r) in raw_routines.iter().enumerate() {
        let i = i + 1;
        if !r.is_object() {
            return Err(t!("第 {i} 个定时任务应该写 name、prompt 或 skill、trigger", i = i));
        }
        let rname = py::head(s_of(&r["name"]).trim(), 40);
        let prompt = py::head(s_of(&r["prompt"]).trim(), 4000);
        let skill = s_of(&r["skill"]).trim().trim_start_matches('/').to_string();
        if rname.is_empty() {
            return Err(t!("第 {i} 个定时任务缺少 name", i = i));
        }
        if prompt.is_empty() && skill.is_empty() {
            return Err(t!("定时任务 {name}：要写 prompt（做什么），或者 skill（用哪个技能）", name = rname));
        }
        if !skill.is_empty() && !seen.contains(&skill) {
            return Err(t!("定时任务 {name}：模板里没有技能 /{skill}", name = rname, skill = skill));
        }
        let raw_trig = if py::truthy(&r["trigger"]) { r["trigger"].clone() } else { json!({}) };
        let trig = clean_trigger(&raw_trig).map_err(|e| t!("定时任务 {name}：{err}", name = rname, err = e))?;
        out_routines.push(json!({"name": rname, "prompt": prompt, "skill": skill, "trigger": trig}));
    }
    let mut uniq: Vec<Value> = vec![];
    for a in apps.as_array().cloned().unwrap_or_default() {
        if !uniq.contains(&a) {
            uniq.push(a);
        }
    }
    let desc = s_of(&t["description"]).split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(json!({"name": name, "description": py::head(&desc, 300), "avatar": avatar, "instructions": instructions,
        "apps": uniq, "skills": out_skills, "routines": out_routines}))
}

/// 给安装前的预览用：应用连没连、定时任务什么时候跑
pub fn preview(t: &Value) -> Value {
    let names: Vec<String> = t["apps"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default();
    let mut out = t.clone();
    out["apps"] = json!(skills::app_status(&names));
    out["routines"] = json!(t["routines"].as_array().cloned().unwrap_or_default().into_iter().map(|mut r| {
        r["when"] = json!(routines::describe(&r["trigger"], None));
        r
    }).collect::<Vec<_>>());
    out
}

// ---------- 内置 ----------
pub fn builtin() -> Vec<Value> {
    let mut out = vec![];
    for (stem, text) in skills::bundled::templates() {
        match parse(&text, None) {
            Ok(t) => out.push(json!({"id": stem, "name": t["name"], "description": t["description"], "avatar": t["avatar"],
                "apps": t["apps"], "skills": t["skills"].as_array().map(|a| a.len()).unwrap_or(0),
                "routines": t["routines"].as_array().map(|a| a.len()).unwrap_or(0)})),
            Err(e) => println!("[模板] {stem}.yaml 有问题：{e}"),
        }
    }
    out
}

pub fn builtin_text(tid: &str) -> Result<String, String> {
    static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9-]{1,40}$").unwrap());
    if !ID.is_match(tid) {
        return Err(t!("没有这个模板"));
    }
    skills::bundled::templates().remove(tid).ok_or_else(|| t!("没有这个模板"))
}

// ---------- 导出 ----------
pub fn from_bot(bot_id: i64) -> Result<Value, String> {
    let b = bots::get(bot_id).ok_or_else(|| t!("没有这个 Bot"))?;
    let rs = routines::list_for(Some(bot_id));
    let used: Vec<String> = rs.iter().filter(|r| py::truthy(&r["skill"])).map(|r| py::str_of(&r["skill"])).collect();
    let ss: Vec<Value> = skills::list_for(bot_id).into_iter().filter(|s| s["bot_id"].as_i64() == Some(bot_id) || used.contains(&py::str_of(&s["name"]))).collect();
    let ins = b["instructions"].as_str().unwrap_or("").to_string();
    let mut body_parts = vec![ins.clone()];
    body_parts.extend(ss.iter().map(|s| py::str_of(&s["body"])));
    body_parts.extend(rs.iter().map(|r| s_of(&r["prompt"])));
    let body = body_parts.join(" ");
    let strip = |c: char| " -#*".contains(c);
    let first = ins.lines().map(|x| x.trim_matches(strip)).find(|x| !x.is_empty()).unwrap_or("").to_string();
    let mut apps: Vec<String> = skills::apps_in(&body);
    for s in &ss {
        if let Some(a) = s["apps"].as_array() {
            apps.extend(a.iter().map(py::str_of));
        }
    }
    apps.sort();
    apps.dedup();
    let mut avatar = json!({});
    for k in ["shape", "color"] {
        if let Some(v) = b["avatar"].get(k) {
            avatar[k] = v.clone();
        }
    }
    let skills_out: Vec<Value> = ss
        .iter()
        .map(|s| {
            let mut o = json!({"name": s["name"], "description": s["description"], "body": s["body"]});
            if !s["apps"].is_null() {
                o["apps"] = s["apps"].clone();
            }
            if py::truthy(&s["files"]) {
                o["files"] = s["files"].clone();
            }
            o
        })
        .collect();
    let routines_out: Vec<Value> = rs
        .iter()
        .map(|r| {
            let mut o = json!({"name": r["name"]});
            if py::truthy(&r["prompt"]) {
                o["prompt"] = r["prompt"].clone();
            }
            if py::truthy(&r["skill"]) {
                o["skill"] = r["skill"].clone();
            }
            o["trigger"] = portable(&r["trigger"]);
            o
        })
        .collect();
    Ok(json!({"osa_template": FORMAT, "name": b["name"], "description": py::head(&first, 120), "avatar": avatar,
        "instructions": ins, "apps": apps, "skills": skills_out, "routines": routines_out}))
}

pub fn to_text(t: &Value) -> String {
    let head = format!("# {}\n", t!("open-super-agent 的 Bot 模板：新建 Bot → 从模板创建 → 导入这个文件"));
    head + &serde_yaml::to_string(t).unwrap_or_default()
}

// ---------- 分享 ----------
pub fn share(bot_id: i64, on: bool) -> Result<Option<String>, String> {
    let b = db::row("select template_token from bots where id=?", params![bot_id]).ok_or_else(|| t!("没有这个 Bot"))?;
    let token = if on { Some(b[0].as_str().map(String::from).unwrap_or_else(|| util::token_urlsafe(16))) } else { None };
    db::exec("update bots set template_token=? where id=?", params![token, bot_id]);
    Ok(token)
}

pub fn token_of(bot_id: i64) -> Option<String> {
    db::row("select template_token from bots where id=?", params![bot_id]).and_then(|r| r[0].as_str().map(String::from))
}

pub fn by_token(token: &str) -> Option<i64> {
    if token.is_empty() {
        return None;
    }
    db::row("select id from bots where template_token=?", params![token]).map(|r| r.i(0))
}

pub fn raw_url(url: &str) -> Result<String, String> {
    let u = reqwest::Url::parse(url.trim()).ok().filter(|u| (u.scheme() == "http" || u.scheme() == "https") && u.host_str().is_some_and(|h| !h.is_empty()));
    let u = u.ok_or_else(|| t!("链接要以 http:// 或 https:// 开头"))?;
    let host = u.host_str().unwrap_or("");
    let netloc = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };
    let parts: Vec<&str> = u.path().split('/').filter(|x| !x.is_empty()).collect();
    if parts.len() == 2 && parts[0] == "template" {
        return Ok(format!("{}://{netloc}/api/shared/templates/{}/raw", u.scheme(), parts[1]));
    }
    if netloc == "github.com" && parts.len() >= 5 && parts[2] == "blob" {
        return Ok(format!("https://raw.githubusercontent.com/{}/{}/{}/{}", parts[0], parts[1], parts[3], parts[4..].join("/")));
    }
    Ok(url.trim().to_string())
}

pub async fn fetch(url: &str) -> Result<Value, String> {
    let src = raw_url(url)?;
    let c = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).redirect(reqwest::redirect::Policy::limited(20)).build().unwrap_or_default();
    let r = c.get(&src).send().await.map_err(|e| {
        let kind = if e.is_timeout() { "ReadTimeout" } else if e.is_connect() { "ConnectError" } else { "HTTPError" };
        t!("取不到这个链接（{err}）", err = kind)
    })?;
    let code = r.status().as_u16();
    if code == 404 {
        return Err(t!("链接打不开（404）：分享可能已经取消了，或者地址不对"));
    }
    if code >= 400 {
        return Err(t!("链接打不开（{code}）", code = code));
    }
    if r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").contains("text/html") {
        return Err(t!("这个链接打开是一个网页，不是模板文件"));
    }
    parse(&r.text().await.unwrap_or_default(), None)
}

// ---------- 安装 ----------
pub fn install(t: &Value, enable_routines: bool, name: &str) -> Result<Value, String> {
    let n = py::head(name.trim(), 40);
    let b = bots::create(if n.is_empty() { t["name"].as_str().unwrap_or("") } else { &n }, &t["avatar"], t["instructions"].as_str().unwrap_or(""), false)?;
    let bid = b["id"].as_i64().unwrap_or(0);
    let r: Result<(), String> = (|| {
        for s in t["skills"].as_array().cloned().unwrap_or_default() {
            let files = if py::truthy(&s["files"]) { s["files"].clone() } else { json!({}) };
            skills::save_full(Some(bid), s["name"].as_str().unwrap_or(""), s["body"].as_str().unwrap_or(""), s["description"].as_str().unwrap_or(""), "import", None, Some(&files), Apps::Set(s["apps"].clone()))?;
        }
        for r in t["routines"].as_array().cloned().unwrap_or_default() {
            let mut trig = r["trigger"].clone();
            if let Some(m) = trig.as_object_mut() {
                m.remove("key");
                m.remove("path");
            }
            let mut data = r.clone();
            data["trigger"] = trig;
            data["enabled"] = json!(enable_routines);
            save_routine(bid, &data)?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        delete_routines(bid);
        db::exec("delete from skills where bot_id=?", params![bid]);
        let _ = bots::delete(bid);
        return Err(e);
    }
    Ok(bots::get(bid).unwrap_or(Value::Null))
}

// ---------- 接口 ----------
fn bot_or_404(bot_id: i64) -> Result<Value, ApiError> {
    bots::get(bot_id).ok_or_else(|| ApiError::not_found(t!("没有这个 Bot")))
}

async fn list_templates() -> Res {
    ok(json!(builtin()))
}

async fn preview_template(Body(d): Body) -> Res {
    let t = if py::truthy(&d["id"]) {
        parse(&builtin_text(&py::str_of(&d["id"]))?, None)?
    } else if !s_of(&d["url"]).trim().is_empty() {
        fetch(&s_of(&d["url"])).await?
    } else {
        parse(&s_of(&d["text"]), None)?
    };
    let mut full = json!({"osa_template": FORMAT});
    for (k, v) in t.as_object().cloned().unwrap_or_default() {
        full[k] = v;
    }
    let mut out = preview(&t);
    out["text"] = json!(to_text(&full));
    ok(out)
}

async fn install_template(Body(d): Body) -> Res {
    let t = parse(&s_of(&d["text"]), None)?;
    let enable = d.get("enable_routines").map(py::truthy).unwrap_or(true);
    let b = install(&t, enable, &s_of(&d["name"]))?;
    for ev in ["bots_changed", "skills_changed", "routines_changed"] {
        broadcast(json!({"type": ev}));
    }
    ok(b)
}

fn template_file(bot_id: i64) -> Response {
    let t = match from_bot(bot_id) {
        Ok(t) => t,
        Err(e) => return ApiError::bad(e).into_response(),
    };
    let name = format!("{}.osabot.yaml", py::str_of(&t["name"]));
    (
        [(header::CONTENT_TYPE, "application/yaml; charset=utf-8".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename*=UTF-8''{}", skills::quote_name(&name)))],
        to_text(&t),
    )
        .into_response()
}

async fn export_template(Path(bot_id): Path<i64>) -> Response {
    if let Err(e) = bot_or_404(bot_id) {
        return e.into_response();
    }
    template_file(bot_id)
}

async fn template_share_state(Path(bot_id): Path<i64>) -> Res {
    bot_or_404(bot_id)?;
    ok(json!({"token": token_of(bot_id)}))
}

async fn share_template(Path(bot_id): Path<i64>, Body(d): Body) -> Res {
    bot_or_404(bot_id)?;
    ok(json!({"token": share(bot_id, py::truthy(&d["on"]))?}))
}

async fn shared_template(Path(token): Path<String>) -> Res {
    let bid = by_token(&token).ok_or_else(|| ApiError::not_found(t!("链接已失效")))?;
    let mut t = preview(&from_bot(bid)?);
    if let Some(apps) = t["apps"].as_array_mut() {
        for a in apps {
            if let Some(m) = a.as_object_mut() {
                m.remove("connected");
            }
        }
    }
    ok(t)
}

async fn shared_template_raw(Path(token): Path<String>) -> Response {
    match by_token(&token) {
        Some(bid) => template_file(bid),
        None => ApiError::not_found(t!("链接已失效")).into_response(),
    }
}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/templates", get(list_templates))
        .route("/api/templates/preview", post(preview_template))
        .route("/api/templates/install", post(install_template))
        .route("/api/bots/{bot_id}/template", get(export_template))
        .route("/api/bots/{bot_id}/template/share", get(template_share_state).post(share_template))
        .route("/api/shared/templates/{token}", get(shared_template))
        .route("/api/shared/templates/{token}/raw", get(shared_template_raw))
}

#[allow(dead_code)]
fn _u() -> (Vec<String>, Vec<String>) {
    (catalog::items().into_keys().collect(), mcp_hub::all_apps())
}
