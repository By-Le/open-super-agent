//! 资料库：Bot 生成的页面（Pages）、小应用的数据、置顶的文件。对应 osa/library.py（说明见那里）。
//!
//! 电脑里的文件（列目录、上传、删除）：接口在 computer/api.rs，这里是对电脑的几步（list_dir / put_file / stat_many / remove）。

use crate::db::{self, RowExt};
use crate::registry::{Call, Out};
use crate::util::normpath;
use crate::web::{ApiError, Body, Res, ok};
use crate::{applets, bots, py, search, server};
use axum::extract::Path;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rusqlite::params;
use serde_json::{Map, Value, json};

pub const FORMATS: [&str; 2] = ["markdown", "html"];
pub const MAX_PAGE: usize = 500_000;
pub const MAX_PAGES: i64 = 1000;
pub const LEGACY: &str = "/data/workspace";
pub const MAX_UPLOAD: usize = 20 << 20;
pub const RAW_CSP: &str = include_str!("prompts/library/raw_csp.txt");

const COLS: &str = "id, bot_id, title, format, share_token, pinned, task_id, created, updated, length(content), app";

pub fn migrate() {
    db::batch(
        "create table if not exists pages(
          id integer primary key, bot_id integer, title text, format text default 'markdown', content text,
          share_token text, pinned integer default 0, task_id integer, created real, updated real, app text);
         create unique index if not exists pages_token on pages(share_token);",
    );
    db::add_column("pages", "app", "text");
    db::batch(
        "create table if not exists page_data(page_id integer, key text, value text, updated real, primary key(page_id, key));
         create table if not exists file_pins(path text primary key, ts real);",
    );
}

// ---------- 页面 ----------
fn page_out(r: &[Value], content: Option<String>) -> Value {
    let r = r.to_vec();
    let app = r.s(10);
    let mut p = json!({"id": r[0], "bot_id": r[1], "title": r[2], "format": r[3], "share_token": r[4], "pinned": py::truthy(&r[5]),
        "task_id": r[6], "created": r[7], "updated": r[8], "size": r.oi(9).unwrap_or(0),
        "app": if app.is_empty() { Value::Null } else { serde_json::from_str(&app).unwrap_or(Value::Null) }});
    if let Some(c) = content {
        p["content"] = json!(c);
    }
    p
}

pub fn list_pages() -> Vec<Value> {
    db::rows(&format!("select {COLS} from pages order by pinned desc, updated desc"), []).iter().map(|r| page_out(r, None)).collect()
}

pub fn get_page(pid: i64, with_content: bool) -> Option<Value> {
    let r = db::row(&format!("select {COLS}, content from pages where id=?"), params![pid])?;
    let content = if with_content { Some(r.s(11)) } else { None };
    Some(page_out(&r[..11], content))
}

pub fn by_token(token: &str) -> Option<Value> {
    if token.is_empty() {
        return None;
    }
    let r = db::row("select id from pages where share_token=?", params![token])?;
    get_page(r.i(0), true)
}

fn clean_title(title: &str) -> String {
    let t = py::head(&title.split_whitespace().collect::<Vec<_>>().join(" "), 120);
    if t.is_empty() { t!("未命名页面") } else { t }
}

/// 新建，或者给 pid 时更新内容。app 是小应用的清单，只对 html 有效；更新时不给就保留原来的
pub fn save_page(
    bot_id: Option<i64>,
    title: &str,
    content: &str,
    fmt: &str,
    pid: Option<i64>,
    task_id: Option<i64>,
    app: &Value,
) -> Result<Value, String> {
    let title = clean_title(title);
    let fmt = if FORMATS.contains(&fmt) { fmt } else { "markdown" };
    let mut app = if fmt == "html" { applets::manifest(app)? } else { None };
    if content.trim().is_empty() {
        return Err(t!("页面内容不能为空"));
    }
    if py::char_len(content) > MAX_PAGE {
        return Err(t!("页面太大（最多 {n}K 字）", n = MAX_PAGE / 1000));
    }
    let now = py::now();
    let pid = match pid {
        Some(pid) => {
            let old = get_page(pid, false).ok_or_else(|| t!("没有页面 #{id}", id = pid))?;
            if app.is_none() && fmt == "html" && !old["app"].is_null() {
                app = Some(old["app"].clone());
            }
            let a = app.as_ref().map(py::dumps_u);
            db::exec(
                "update pages set title=?, format=?, content=?, updated=?, app=? where id=?",
                params![title, fmt, content, now, a, pid],
            );
            pid
        }
        None => {
            if db::scalar_i64("select count(*) from pages", []) >= MAX_PAGES {
                return Err(t!("资料库最多 {n} 个页面，先删掉一些", n = MAX_PAGES));
            }
            let a = app.as_ref().map(py::dumps_u);
            db::insert(
                "insert into pages(bot_id, title, format, content, task_id, created, updated, app) values(?,?,?,?,?,?,?,?)",
                params![bot_id, title, fmt, content, task_id, now, now, a],
            )
        }
    };
    Ok(get_page(pid, false).unwrap_or(Value::Null))
}

pub fn update_page(pid: i64, title: Option<&str>, pinned: Option<bool>) -> Result<Value, String> {
    if get_page(pid, false).is_none() {
        return Err(t!("没有页面 #{id}", id = pid));
    }
    if let Some(t) = title {
        db::exec("update pages set title=?, updated=? where id=?", params![clean_title(t), py::now(), pid]);
    }
    if let Some(p) = pinned {
        db::exec("update pages set pinned=? where id=?", params![p as i64, pid]);
    }
    Ok(get_page(pid, false).unwrap_or(Value::Null))
}

/// 打开分享时生成新链接；关掉后旧链接立即失效
pub fn share(pid: i64, on: bool) -> Value {
    let token = if on { Some(crate::util::token_urlsafe(16)) } else { None };
    db::exec("update pages set share_token=? where id=?", params![token, pid]);
    get_page(pid, false).unwrap_or(Value::Null)
}

pub fn delete_page(pid: i64) {
    db::exec("delete from pages where id=?", params![pid]);
    db::exec("delete from page_data where page_id=?", params![pid]);
}

// ---------- 小应用的数据 ----------
pub fn get_data(pid: i64) -> Map<String, Value> {
    db::rows("select key, value from page_data where page_id=? order by key", params![pid])
        .into_iter()
        .map(|r| (r.s(0), serde_json::from_str(&r.s(1)).unwrap_or(Value::Null)))
        .collect()
}

/// 合并写入：值为 null 的键删掉。返回全部数据
pub fn set_data(pid: i64, updates: &Value) -> Result<Map<String, Value>, String> {
    let cur = get_data(pid);
    let new = applets::check_data(updates, &cur)?;
    let now = py::now();
    for k in cur.keys() {
        if !new.contains_key(k) {
            db::exec("delete from page_data where page_id=? and key=?", params![pid, k]);
        }
    }
    for (k, v) in &new {
        if cur.get(k) != Some(v) {
            db::exec(
                "insert or replace into page_data(page_id, key, value, updated) values(?,?,?,?)",
                params![pid, k, py::dumps_u(v), now],
            );
        }
    }
    Ok(new)
}

/// 对话里引用页面用的地址，前端会在资料库里打开它
pub fn link(pid: i64) -> String {
    format!("/library/pages/{pid}")
}

// ---------- 工具 ----------
pub async fn tool_page_data(c: Call) -> Out {
    let a = &c.args;
    let pid = &a["page_id"];
    let p = if pid.is_i64() { get_page(pid.as_i64().unwrap(), false) } else { None };
    let Some(p) = p else { return format!("没有页面 #{}", py::str_of(pid)).into() };
    let pid = p["id"].as_i64().unwrap_or(0);
    if p["app"].is_null() {
        return format!("页面 #{pid} 不是小应用，没有数据。要做成小应用，用 save_page 更新它并填 app").into();
    }
    let (data, head) = match a.get("set").filter(|v| !v.is_null()) {
        Some(upd) => {
            if p["bot_id"].as_i64() != Some(c.bot_id()) {
                return format!("没写：小应用 #{pid} 不是你建的，只能读").into();
            }
            let data = match set_data(pid, upd) {
                Ok(d) => d,
                Err(e) => return format!("没写：{e}").into(),
            };
            server::broadcast(json!({"type": "page_data", "page_id": pid, "data": data}));
            let n = upd.as_object().map(|o| o.len()).unwrap_or(0);
            (data, format!("已写入 {n} 个键。"))
        }
        None => (get_data(pid), String::new()),
    };
    let text = py::dumps_indent(&Value::Object(data.clone()), 1, false);
    let more = if py::char_len(&text) > 12000 { "\n…（太长，截断）" } else { "" };
    format!("{head}小应用「{}」的数据（{} 个键）：\n{}{more}", py::str_of(&p["title"]), data.len(), py::head(&text, 12000)).into()
}

pub fn read_page_text(args: &Value, limit: usize) -> String {
    let id = &args["page_id"];
    let p = if id.is_i64() { get_page(id.as_i64().unwrap(), true) } else { None };
    let Some(p) = p else { return format!("没有页面 #{}", py::str_of(id)) };
    let fmt = p["format"].as_str().unwrap_or("");
    let mut text = search::plain(p["content"].as_str().unwrap_or(""), fmt);
    if fmt == "html" {
        static PARA: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"\n\s*\n+").unwrap());
        static SP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[ \t]+").unwrap());
        text = SP.replace_all(&PARA.replace_all(&text, "\n\n"), " ").trim().to_string();
    }
    let n = py::char_len(&text);
    let more = if n > limit { format!("\n…（还有 {} 字没显示）", n - limit) } else { String::new() };
    format!(
        "《{}》{}，{} 更新\n\n{}{more}",
        py::str_of(&p["title"]),
        link(p["id"].as_i64().unwrap_or(0)),
        py::strftime_at(p["updated"].as_f64().unwrap_or(0.0), "%Y-%m-%d"),
        py::head(&text, limit)
    )
}

pub async fn tool_read_page(c: Call) -> Out {
    read_page_text(&c.args, 12000).into()
}

pub fn save_page_text(bot_id: i64, args: &Value, task_id: Option<i64>) -> String {
    let pid = &args["page_id"];
    let pid_i = if pid.is_null() { None } else { Some(pid.as_i64()) };
    if let Some(p) = pid_i {
        let old = p.and_then(|p| get_page(p, false));
        if old.as_ref().is_none_or(|o| o["bot_id"].as_i64() != Some(bot_id)) {
            return format!("没保存：页面 #{} 不存在或不是你建的，只能更新自己的页面", py::str_of(pid));
        }
    }
    let app = &args["app"];
    if py::truthy(app)
        && let Some(o) = app.as_object()
    {
        let unknown: Vec<String> = o
            .get("tools")
            .and_then(|t| t.as_array())
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str())
            .filter(|t| !applets::known(t))
            .map(String::from)
            .collect();
        if !unknown.is_empty() {
            return format!("没保存：app.tools 里有现在没有的工具 {}，只能列你这一轮能用的工具", unknown.join(", "));
        }
    }
    let s = |k: &str| args[k].as_str().unwrap_or("").to_string();
    let fmt = if py::truthy(&args["format"]) { py::str_of(&args["format"]) } else { "markdown".into() };
    let p = match save_page(Some(bot_id), &s("title"), &s("content"), &fmt, pid_i.flatten(), task_id, app) {
        Ok(p) => p,
        Err(e) => return format!("没保存：{e}"),
    };
    let id = p["id"].as_i64().unwrap_or(0);
    let title = py::str_of(&p["title"]);
    if !p["app"].is_null() {
        return format!(
            "已保存小应用 #{id}「{title}」，链接 {}。回复里用 Markdown 链接附上它，对话里会直接显示这个小应用，用户能在里面操作。用 page_data 能读写它的数据（page_id={id}）。",
            link(id)
        );
    }
    format!("已保存页面 #{id}「{title}」，链接 {}（用户在资料库里也能看到）。回复里用 Markdown 链接附上它。", link(id))
}

pub async fn tool_save_page(c: Call) -> Out {
    let task_id = c.task.as_ref().filter(|t| t.fake_id.is_none()).map(|t| t.id);
    let out = save_page_text(c.bot_id(), &c.args, task_id);
    server::broadcast(json!({"type": "pages_changed"}));
    out.into()
}

// ---------- 文件 ----------
/// 现在这台电脑的工作文件夹
pub fn root() -> String {
    crate::computer::get(0).workspace()
}

/// 把路径规范成工作文件夹下的绝对路径（相对路径从工作文件夹算起）；跑出去就报错
pub fn norm(path: &str, base: Option<&str>) -> Result<String, String> {
    let base = base.map(String::from).unwrap_or_else(root);
    let mut p = path.trim().to_string();
    if p.is_empty() {
        p = base.clone();
    }
    if base != LEGACY && (p == LEGACY || p.starts_with(&format!("{LEGACY}/"))) {
        p = format!("{base}{}", &p[LEGACY.len()..]);
    }
    if !p.starts_with('/') {
        p = if base.ends_with('/') { format!("{base}{p}") } else { format!("{base}/{p}") };
    }
    let p = normpath(&p);
    if p != base && !p.starts_with(&format!("{}/", base.trim_end_matches('/'))) {
        return Err(t!("只能访问 {dir} 里的文件", dir = base));
    }
    Ok(p)
}

pub fn check_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() || n == "." || n == ".." || n.contains('/') || n.contains('\0') || py::char_len(n) > 200 {
        return Err(t!("文件名不合法"));
    }
    Ok(n.to_string())
}

pub async fn list_dir(c: &crate::computer::Computer, path: &str) -> Result<Vec<Value>, String> {
    let mut out = c.ls(&norm(path, Some(&c.workspace()))?).await?;
    out.sort_by_key(|e| (e["dir"] != true, py::str_of(&e["name"]).to_lowercase()));
    Ok(out)
}

/// 把文件写进电脑里的目录。同名时自动加序号，不覆盖
pub async fn put_file(c: &crate::computer::Computer, directory: &str, name: &str, data: &[u8]) -> Result<Value, String> {
    let d = norm(directory, Some(&c.workspace()))?;
    let name = check_name(name)?;
    if data.len() > MAX_UPLOAD {
        return Err(t!("文件太大（最多 {n} MB）", n = MAX_UPLOAD >> 20));
    }
    let existing: Vec<String> = list_dir(c, &d).await.map(|v| v.iter().map(|e| py::str_of(&e["name"])).collect()).unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    let (mut fin, mut n) = (name.clone(), 1);
    while existing.contains(&fin) {
        n += 1;
        fin = format!("{stem} ({n}){ext}");
    }
    let path = crate::computer::base::join(&d, &fin);
    c.write_bytes(&path, data).await.map_err(|e| e.0)?;
    Ok(json!({"path": path, "name": fin, "size": data.len()}))
}

/// 置顶文件的现状（可能已经被删了）
pub async fn stat_many(c: &crate::computer::Computer, paths: &[String]) -> Result<indexmap::IndexMap<String, Value>, String> {
    if paths.is_empty() {
        return Ok(Default::default());
    }
    c.stat_paths(paths).await.map_err(|e| e.0)
}

pub async fn remove(c: &crate::computer::Computer, path: &str) -> Result<(), String> {
    let p = norm(path, Some(&c.workspace()))?;
    if p == c.workspace() {
        return Err(t!("不能删除整个工作目录"));
    }
    c.rm(&p).await?;
    unpin(&p)
}

pub fn pins() -> Vec<String> {
    let base = format!("{}/", root().trim_end_matches('/'));
    db::rows("select path from file_pins order by ts", [])
        .into_iter()
        .map(|r| r.s(0))
        .filter(|p| format!("{p}/").starts_with(&base))
        .collect()
}

pub fn pin(path: &str) -> Result<(), String> {
    db::exec("insert or replace into file_pins values(?, ?)", params![norm(path, None)?, py::now()]);
    Ok(())
}

pub fn unpin(path: &str) -> Result<(), String> {
    let p = norm(path, None)?;
    db::exec("delete from file_pins where path=? or path like ?", params![p, format!("{p}/%")]);
    Ok(())
}

// ---------- 接口 ----------
fn page_or_404(pid: i64, content: bool) -> Result<Value, ApiError> {
    get_page(pid, content).ok_or_else(|| ApiError::not_found(t!("没有这个页面")))
}

fn applet_or_404(pid: i64) -> Result<Value, ApiError> {
    let p = page_or_404(pid, false)?;
    if p["app"].is_null() {
        return Err(ApiError::bad(t!("页面 #{id} 不是小应用", id = pid)));
    }
    Ok(p)
}

/// html 页面的原文，带 CSP：在沙箱里运行，碰不到这个站点的接口和数据
pub fn raw(page: &Value) -> Response {
    if page["format"] != "html" {
        return ApiError::not_found(t!("只有 html 页面有原文地址")).into_response();
    }
    let content = page["content"].as_str().unwrap_or("");
    let html = if page["app"].is_null() { content.to_string() } else { applets::inject(content) };
    let mut r = ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
    let h = r.headers_mut();
    h.insert("content-security-policy", HeaderValue::from_static(RAW_CSP));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    r
}

async fn api_list() -> Res {
    ok(json!(list_pages()))
}

async fn api_create(Body(d): Body) -> Res {
    let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
    let fmt = if py::truthy(&d["format"]) { py::str_of(&d["format"]) } else { "markdown".into() };
    let p = save_page(d["bot_id"].as_i64(), &s("title"), &s("content"), &fmt, None, d["task_id"].as_i64(), &d["app"])?;
    server::broadcast(json!({"type": "pages_changed"}));
    ok(p)
}

async fn api_get(Path(pid): Path<i64>) -> Res {
    ok(page_or_404(pid, true)?)
}

async fn api_raw(Path(pid): Path<i64>) -> Response {
    match page_or_404(pid, true) {
        Ok(p) => raw(&p),
        Err(e) => e.into_response(),
    }
}

async fn api_update(Path(pid): Path<i64>, Body(d): Body) -> Res {
    page_or_404(pid, false)?;
    let pinned = if d["pinned"].is_null() { None } else { Some(py::truthy(&d["pinned"])) };
    let p = update_page(pid, d["title"].as_str(), pinned)?;
    server::broadcast(json!({"type": "pages_changed"}));
    ok(p)
}

async fn api_share(Path(pid): Path<i64>, Body(d): Body) -> Res {
    page_or_404(pid, false)?;
    let p = share(pid, py::truthy(&d["on"]));
    server::broadcast(json!({"type": "pages_changed"}));
    ok(p)
}

async fn api_delete(Path(pid): Path<i64>) -> Res {
    delete_page(pid);
    server::broadcast(json!({"type": "pages_changed"}));
    ok(json!({"ok": true}))
}

async fn api_data(Path(pid): Path<i64>) -> Res {
    applet_or_404(pid)?;
    ok(Value::Object(get_data(pid)))
}

async fn api_put_data(Path(pid): Path<i64>, Body(d): Body) -> Res {
    applet_or_404(pid)?;
    let out = set_data(pid, &d)?;
    server::broadcast(json!({"type": "page_data", "page_id": pid, "data": out}));
    ok(Value::Object(out))
}

async fn api_call(Path(pid): Path<i64>, Body(d): Body) -> Res {
    let p = applet_or_404(pid)?;
    let args = if py::truthy(&d["args"]) { d["args"].clone() } else { json!({}) };
    let tool = if d["tool"].is_null() { String::new() } else { py::str_of(&d["tool"]) };
    match applets::call(&p, &tool, &args).await {
        Ok(r) => ok(json!({"result": r})),
        Err((code, msg)) => Err(ApiError::new(code, msg)),
    }
}

async fn api_say(Path(pid): Path<i64>, Body(d): Body) -> Res {
    let p = applet_or_404(pid)?;
    let text = if d["text"].is_null() { String::new() } else { py::str_of(&d["text"]) };
    applets::say(&p, &text).map_err(|(c, m)| ApiError::new(c, m))?;
    ok(json!({"ok": true}))
}

async fn api_shared(Path(token): Path<String>) -> Res {
    let p = by_token(&token).ok_or_else(|| ApiError::not_found(t!("链接已失效")))?;
    let mut out = json!({"title": p["title"], "format": p["format"], "content": p["content"], "updated": p["updated"]});
    if let Some(b) = p["bot_id"].as_i64().filter(|b| *b != 0).and_then(bots::get) {
        out["bot"] = json!({"name": b["name"], "avatar": b["avatar"]});
    }
    ok(out)
}

async fn api_shared_raw(Path(token): Path<String>) -> Response {
    match by_token(&token) {
        Some(p) => raw(&p),
        None => ApiError::not_found(t!("链接已失效")).into_response(),
    }
}

async fn api_pin(Body(d): Body) -> Res {
    let path = d["path"].as_str().unwrap_or("");
    if py::truthy(&d["on"]) {
        pin(path)?
    } else {
        unpin(path)?
    }
    ok(json!({"ok": true, "pins": pins()}))
}

/// /api/pages…、/api/shared/{token}…、/api/files/pin
pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/pages", get(api_list).post(api_create))
        .route("/api/pages/{pid}", get(api_get).patch(api_update).delete(api_delete))
        .route("/api/pages/{pid}/raw", get(api_raw))
        .route("/api/pages/{pid}/share", post(api_share))
        .route("/api/pages/{pid}/data", get(api_data).put(api_put_data))
        .route("/api/pages/{pid}/call", post(api_call))
        .route("/api/pages/{pid}/say", post(api_say))
        .route("/api/shared/{token}", get(api_shared))
        .route("/api/shared/{token}/raw", get(api_shared_raw))
        .route("/api/files/pin", post(api_pin))
}
