//! 技能：可复用的做事方法。用户在对话里用 /名字 调用；Bot 觉得某个技能管这件事时，自己用 use_skill 取做法照着做。
//! 对应 osa/skills.py（技能包的格式、导入导出、分享、技能市场的说明见那里）。
//!
//! 技能市场的文件编进二进制（assets/skills.json，server/scripts/dump_skills.py 生成）。

use crate::db::{self, RowExt};
use crate::registry::{Call, Out};
use crate::server::broadcast;
use crate::web::{ApiError, Body, Res, int, ok};
use crate::{bots, catalog, i18n, mcp_hub, py, util};
use axum::extract::Path;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::{get as route_get, post};
use regex::Regex;
use rusqlite::params;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub const MAX_PER_BOT: i64 = 100;
pub const MAX_BODY: usize = 20000;
pub const MAX_FILE: usize = 200_000;
pub const MAX_FILES: usize = 50;
pub const MAX_FILES_TOTAL: usize = 200_000;
pub const MAX_ZIP: usize = 2_000_000;
pub const PREVIEW: usize = 400;
pub const CHUNK: usize = 7000;

static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w一-鿿-]{1,40}$").unwrap());
static MARKET_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,39}$").unwrap());
static APP_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]{0,23}$").unwrap());
static PART: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^[^/\\\x00-\x1f:*?"<>|]{1,80}$"#).unwrap());
static SKILL_MD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^skill(\.[a-z]{2})?\.md$").unwrap());
static BINARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(png|jpe?g|gif|webp|ico|pdf|zip|gz|tgz|bz2|xz|7z|rar|ttf|otf|woff2?|eot|mp[34]|mov|wav|exe|dll|so|dylib|bin|class|jar|pyc|docx?|xlsx?|pptx?|sqlite|db)$").unwrap()
});
static APP_REF: LazyLock<fancy_regex::Regex> = LazyLock::new(|| fancy_regex::Regex::new(r"(?<![\w@.])@([a-z][a-z0-9_-]{0,23})\b").unwrap());

pub fn migrate() {
    db::batch(
        "create table if not exists skills(
          id integer primary key, bot_id integer, name text, description text default '', body text,
          source text default 'manual', created real, updated real)",
    );
    for col in ["share_token", "files", "apps"] {
        db::add_column("skills", col, "text");
    }
    db::batch("create unique index if not exists skills_token on skills(share_token)");
}

const COLS: &str = "id, bot_id, name, description, body, source, created, updated, share_token, files, apps";

fn row_out(r: &[Value]) -> Value {
    let r = r.to_vec();
    let files = match r.get(9) {
        Some(Value::String(s)) if !s.is_empty() => serde_json::from_str(s).unwrap_or(json!({})),
        _ => json!({}),
    };
    let apps = match r.get(10) {
        Some(Value::String(s)) if !s.is_empty() => serde_json::from_str(s).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    json!({"id": r[0], "bot_id": r[1], "name": r[2], "description": r.s(3), "body": r.s(4), "source": r[5],
           "created": r[6], "updated": r[7], "share_token": r[8], "files": files, "apps": apps})
}

/// 这个 Bot 能用的技能（自己的 + 共用的），同名时自己的覆盖共用的
pub fn list_for(bot_id: i64) -> Vec<Value> {
    let rows =
        db::rows(&format!("select {COLS} from skills where bot_id=? or bot_id is null order by bot_id is null, name"), params![bot_id]);
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for r in rows {
        let s = row_out(&r);
        let n = py::str_of(&s["name"]);
        if seen.insert(n) {
            out.push(s);
        }
    }
    out
}

pub fn get(sid: i64) -> Option<Value> {
    db::row(&format!("select {COLS} from skills where id=?"), params![sid]).map(|r| row_out(&r))
}

pub fn find(bot_id: i64, name: &str) -> Option<Value> {
    list_for(bot_id).into_iter().find(|s| s["name"] == name)
}

/// 技能名和做法的检查，返回整理过的名字
pub fn check(name: &str, body: &str) -> Result<String, String> {
    let name = name.trim().trim_start_matches('/').to_string();
    if !NAME_RE.is_match(&name) {
        return Err(t!("技能名只能用中文、字母、数字、- 和 _，最长 40 个字"));
    }
    if body.trim().is_empty() {
        return Err(t!("技能内容不能为空"));
    }
    let n = py::char_len(body);
    if n > MAX_BODY {
        return Err(t!("技能做法太长了（{count} 个字），最多 {max} 个字", count = n, max = MAX_BODY));
    }
    Ok(name)
}

/// 技能包里文件的相对路径：统一成 a/b.py；绝对路径、..、以 . 开头的都不要，返回空
pub fn clean_path(p: &str) -> String {
    let mut p = p.replace('\\', "/").trim().to_string();
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    if p.is_empty() || py::char_len(&p) > 200 || p.starts_with('/') {
        return String::new();
    }
    if p.split('/').all(|x| !x.starts_with('.') && PART.is_match(x) && x == x.trim()) { p } else { String::new() }
}

/// 带的文件：{相对路径: 文字}，按路径排好
pub fn check_files(files: &Value) -> Result<Map<String, Value>, String> {
    if !py::truthy(files) {
        return Ok(Map::new());
    }
    let Some(obj) = files.as_object() else {
        return Err(t!("技能带的文件应该是「相对路径: 文字内容」"));
    };
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut total = 0;
    for (raw, text) in obj {
        let path = clean_path(raw);
        if path.is_empty() {
            return Err(t!("文件路径不对：{path}（要用相对路径，不能有 ..，也不能以 . 开头）", path = py::head(raw, 80)));
        }
        if SKILL_MD.is_match(&path) {
            return Err(t!("SKILL.md 就是做法本身，不用再作为文件带上"));
        }
        let Some(t) = text.as_str().filter(|t| !t.contains('\0')) else {
            return Err(t!("{path} 不是文字文件：技能只能带文字文件（脚本、Markdown、JSON……）", path = path));
        };
        total += py::char_len(t);
        out.insert(path, t.to_string());
    }
    if out.len() > MAX_FILES {
        return Err(t!("带的文件太多了（{count} 个），最多 {max} 个", count = out.len(), max = MAX_FILES));
    }
    if total > MAX_FILES_TOTAL {
        return Err(t!("带的文件太大了（合计 {count} 个字），最多 {max} 个字", count = total, max = MAX_FILES_TOTAL));
    }
    Ok(out.into_iter().map(|(k, v)| (k, Value::String(v))).collect())
}

/// frontmatter 里的 apps：列表，或者用逗号、空格分开的一串
pub fn clean_apps(apps: &Value) -> Vec<String> {
    static SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s,，、]+").unwrap());
    let items: Vec<String> = match apps {
        Value::String(s) => SPLIT.split(s).map(String::from).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
                Value::Bool(_) => None, // YAML 的 true 不是应用名
                _ => None,
            })
            .collect(),
        _ => return vec![],
    };
    let mut out: Vec<String> = vec![];
    for a in items {
        let a = a.trim().trim_start_matches('@').to_lowercase();
        if APP_ID.is_match(&a) && !out.contains(&a) {
            out.push(a);
        }
    }
    out.truncate(20);
    out
}

/// apps 参数：不改 / 没声明 / 声明了这些
pub enum Apps {
    Keep,
    Set(Value),
}

/// 新建或修改。同一个 Bot 下同名的技能会被覆盖。files 不给、apps=Keep 时：修改的保持原样，新建的没有
#[allow(clippy::too_many_arguments)]
pub fn save_full(
    bot_id: Option<i64>,
    name: &str,
    body: &str,
    description: &str,
    source: &str,
    sid: Option<i64>,
    files: Option<&Value>,
    apps: Apps,
) -> Result<Value, String> {
    let name = check(name, body)?;
    let mut cols: Vec<(&str, Option<String>)> = vec![];
    if let Some(f) = files {
        let f = check_files(f)?;
        cols.push(("files", if f.is_empty() { None } else { Some(py::dumps_u(&Value::Object(f))) }));
    }
    if let Apps::Set(a) = &apps {
        cols.push(("apps", if a.is_null() { None } else { Some(py::dumps(&json!(clean_apps(a)), true)) }));
    }
    let get_col = |k: &str| cols.iter().find(|(c, _)| *c == k).and_then(|(_, v)| v.clone());
    let now = py::now();
    let desc = py::head(description, 200);
    let sid = match sid {
        Some(s) => Some(s),
        None => db::row("select id from skills where name=? and bot_id is ?", params![name, bot_id]).map(|r| r.i(0)),
    };
    let sid = match sid {
        None => {
            let n = db::scalar_i64("select count(*) from skills where bot_id is ?", params![bot_id]);
            if n >= MAX_PER_BOT {
                return Err(t!("每个 Bot 最多 {n} 个技能", n = MAX_PER_BOT));
            }
            db::insert(
                "insert into skills(bot_id, name, description, body, source, created, updated, files, apps) values(?,?,?,?,?,?,?,?,?)",
                params![bot_id, name, desc, body, source, now, now, get_col("files"), get_col("apps")],
            )
        }
        Some(sid) => {
            if db::exists("select id from skills where name=? and bot_id is ? and id != ?", params![name, bot_id, sid]) {
                return Err(t!("已经有叫 /{name} 的技能了", name = name));
            }
            db::exec(
                "update skills set bot_id=?, name=?, description=?, body=?, updated=? where id=?",
                params![bot_id, name, desc, body, now, sid],
            );
            for (k, v) in &cols {
                db::exec(&format!("update skills set {k}=? where id=?"), params![v, sid]);
            }
            sid
        }
    };
    Ok(get(sid).unwrap_or(Value::Null))
}

/// Router 的 save_skill 用（Bot 自己的、不带文件）
pub fn save(bot_id: i64, name: &str, body: &str, desc: &str, source: &str) -> Result<Value, String> {
    save_full(Some(bot_id), name, body, desc, source, None, None, Apps::Keep)
}

pub fn delete(sid: i64) {
    db::exec("delete from skills where id=?", params![sid]);
}

/// 消息以 /技能名 开头时返回 (技能, 其余文字)
pub fn parse(bot_id: i64, text: &str) -> Option<(Value, String)> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/([\w一-鿿-]+)(?:\s+([\s\S]*))?$").unwrap());
    let c = RE.captures(text.trim())?;
    let s = find(bot_id, &c[1])?;
    Some((s, c.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default()))
}

fn file_names(s: &Value) -> Vec<String> {
    s["files"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default()
}

/// 把技能写成交给模型的说明。by_user：用户用 /名字 调用的（false 是 Bot 自己用 use_skill 取的）
pub fn expand_by(skill: &Value, extra: &str, by_user: bool) -> String {
    let name = py::str_of(&skill["name"]);
    let head = if by_user {
        format!("（用户调用了技能 /{name}，请按下面的做法执行；用户补充的要求优先。）")
    } else {
        format!("（技能 /{name} 的做法，按它执行；和用户这次的要求冲突时以用户的为准。）")
    };
    let files = file_names(skill);
    let mut out = format!("{head}\n--- 技能做法 ---\n{}\n--- 结束 ---", skill["body"].as_str().unwrap_or(""));
    if by_user && !files.is_empty() {
        out += &format!("\n这个技能带了文件：{}。要看全文或运行，用 use_skill(name=\"{name}\", file=\"路径\") 取。", files.join("、"));
    }
    if !extra.is_empty() {
        out += &format!("\n用户补充：{extra}");
    }
    out
}

pub fn expand(skill: &Value, extra: &str) -> String {
    expand_by(skill, extra, true)
}

pub fn summary(bot_id: i64) -> String {
    let lines: Vec<String> = list_for(bot_id)
        .iter()
        .map(|s| {
            let d = s["description"].as_str().unwrap_or("");
            format!(
                "- /{}：{}",
                py::str_of(&s["name"]),
                if d.is_empty() { py::head(s["body"].as_str().unwrap_or(""), 40) } else { d.to_string() }
            )
        })
        .collect();
    if lines.is_empty() { "暂无技能".into() } else { lines.join("\n") }
}

// ---------- use_skill 工具 ----------
fn piece(text: &str, offset: &Value, label: &str, again: &str) -> (String, bool) {
    let off = py::to_int(if offset.is_null() { &Value::Null } else { offset }).unwrap_or(0).max(0) as usize;
    let chars: Vec<char> = text.chars().collect();
    let start = off.min(chars.len());
    let end = (off + CHUNK).min(chars.len());
    let part: String = chars[start..end].iter().collect();
    let plen = end - start;
    let total = chars.len();
    let more = total as i64 - off as i64 - plen as i64;
    let wh = if off > 0 || more != 0 { format!("，这是第 {}–{} 字", off + 1, off + plen) } else { String::new() };
    let mut out = format!("--- {label}（共 {total} 字{wh}）---\n{part}\n--- 结束 ---");
    if more > 0 {
        out += &format!("\n还有 {more} 字没给：用 {} 取下一段。", again.replace("{offset}", &(off + plen).to_string()));
    }
    (out, more > 0)
}

fn files_block(s: &Value, scope: &str) -> String {
    let Some(files) = s["files"].as_object().filter(|f| !f.is_empty()) else { return String::new() };
    let name = py::str_of(&s["name"]);
    let mut lines = vec![format!("\n\n这个技能带了 {} 个文件（附上每个文件开头 {PREVIEW} 字）：", files.len())];
    let mut budget: i64 = 5000;
    for (p, text) in files {
        let text = text.as_str().unwrap_or("");
        let n = py::char_len(text);
        if budget > 0 {
            let head = py::head(text, PREVIEW);
            budget -= py::char_len(&head) as i64;
            lines.push(format!("--- {p}（{n} 字）---\n{head}{}", if n > PREVIEW { "……" } else { "" }));
        } else {
            lines.push(format!("--- {p}（{n} 字，开头没附上）---"));
        }
    }
    lines.push(format!("要看某个文件的全文：use_skill(name=\"{name}\", file=\"路径\")。"));
    if scope == "router" {
        lines.push(format!("要运行里面的脚本就派任务（dispatch），goal 里写明按技能 /{name} 做、跑哪个脚本，Worker 会自己取文件。"));
    } else {
        lines.push("要运行脚本，用 computer_write_file 把文件内容写进工作文件夹再用 run_code 或 computer_shell 跑；参考资料看全文就行，不用写进电脑。".into());
    }
    lines.join("\n")
}

pub fn tool_use(bot_id: i64, scope: &str, args: &Value) -> String {
    let name = crate::builtin::arg_s(args, "name").trim().trim_start_matches('/').to_string();
    let Some(s) = find(bot_id, &name) else {
        let have = list_for(bot_id).iter().map(|x| format!("/{}", py::str_of(&x["name"]))).collect::<Vec<_>>().join(" ");
        return format!("没有 /{name} 这个技能。能用的：{}", if have.is_empty() { "无" } else { &have });
    };
    let raw = crate::builtin::arg_s(args, "file").trim().to_string();
    if !raw.is_empty() {
        let path = clean_path(&raw);
        let files = s["files"].as_object().cloned().unwrap_or_default();
        let Some(text) = files.get(&path) else {
            let fs = file_names(&s).join("、");
            return format!("/{name} 没有 {raw} 这个文件。它带的文件：{}", if fs.is_empty() { "无" } else { &fs });
        };
        return piece(
            text.as_str().unwrap_or(""),
            &args["offset"],
            &path,
            &format!("use_skill(name=\"{name}\", file=\"{path}\", offset={{offset}})"),
        )
        .0;
    }
    let out = expand_by(&s, "", false);
    if py::char_len(&out) > CHUNK || py::truthy(&args["offset"]) {
        let (out, more) = piece(
            s["body"].as_str().unwrap_or(""),
            &args["offset"],
            &format!("技能 /{name} 的做法"),
            &format!("use_skill(name=\"{name}\", offset={{offset}})"),
        );
        return if more { out } else { out + &files_block(&s, scope) };
    }
    out + &files_block(&s, scope)
}

pub async fn tool_use_skill(c: Call) -> Out {
    Out::Text(tool_use(c.bot_id(), c.scope, &c.args))
}

/// 定时任务、事件按技能跑时附在做法后面
pub fn files_hint(skill: &Value) -> String {
    let files = file_names(skill);
    if files.is_empty() {
        return String::new();
    }
    format!(
        "\n这个技能带了文件：{}。要看全文用 use_skill(name=\"{}\", file=\"路径\")；要运行脚本，用 computer_write_file 把文件内容写进工作文件夹再用 run_code 或 computer_shell 跑。",
        files.join("、"),
        py::str_of(&skill["name"])
    )
}

// ---------- 导出、导入、分享 ----------
/// 做法里 @ 到的应用（应用市场里有的，或者本机装了的）
pub fn apps_in(body: &str) -> Vec<String> {
    let mut known: std::collections::HashSet<String> = catalog::items().into_keys().collect();
    known.extend(mcp_hub::all_apps());
    let mut out: Vec<String> = APP_REF
        .captures_iter(body)
        .filter_map(|c| c.ok())
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|m| known.contains(m))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 技能要用的应用：frontmatter 里写了 apps 就以它为准，没写才按做法里的 @名字 找
pub fn apps_of(skill: &Value) -> Vec<String> {
    match &skill["apps"] {
        Value::Array(a) => a.iter().map(py::str_of).collect(),
        _ => apps_in(skill["body"].as_str().unwrap_or("")),
    }
}

fn yaml_dump(v: &Value) -> String {
    serde_yaml::to_string(v).unwrap_or_default()
}

pub fn to_file(skill: &Value) -> String {
    let mut head = json!({"name": skill["name"], "description": skill["description"].as_str().unwrap_or("")});
    let apps = apps_of(skill);
    if !apps.is_empty() || !skill["apps"].is_null() {
        head["apps"] = json!(apps);
    }
    let meta = yaml_dump(&head);
    format!("---\n{}\n---\n\n{}\n", meta.trim(), skill["body"].as_str().unwrap_or("").trim())
}

pub fn to_zip(skill: &Value) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let _ = z.start_file("SKILL.md", opt);
        let _ = z.write_all(to_file(skill).as_bytes());
        if let Some(files) = skill["files"].as_object() {
            for (p, t) in files {
                let _ = z.start_file(p.as_str(), opt);
                let _ = z.write_all(t.as_str().unwrap_or("").as_bytes());
            }
        }
        let _ = z.finish();
    }
    buf.into_inner()
}

/// 导出成文件：(内容, 文件名, 类型)。带文件的是 zip，不带的是 .md
pub fn export(skill: &Value) -> (Vec<u8>, String, &'static str) {
    let name = py::str_of(&skill["name"]);
    if py::truthy(&skill["files"]) {
        return (to_zip(skill), format!("{name}.zip"), "application/zip");
    }
    (to_file(skill).into_bytes(), format!("{name}.md"), "text/markdown; charset=utf-8")
}

/// 别处的技能名可能有空格、点：换成 -，去掉不能用的字
pub fn clean_name(name: &str) -> String {
    static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\s.]+").unwrap());
    static BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w一-鿿-]").unwrap());
    let n = WS.replace_all(name.trim().trim_start_matches('/'), "-").into_owned();
    let n = BAD.replace_all(&n, "").into_owned();
    py::head(n.trim_matches(|c| c == '-' || c == '_'), 40)
}

fn load_yaml(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_yaml::from_str::<serde_yaml::Value>(text)
        .map(crate::config::yaml_to_json)
        .map_err(|e| e.to_string().lines().next().unwrap_or("").to_string())
}

/// 解析一个技能文件，返回 {name, description, body, apps, files}
pub fn from_file(text: &str, filename: &str) -> Result<Value, String> {
    static FRONT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^---[ \t]*\n(.*?)\n---[ \t]*(?:\n|$)").unwrap());
    static EXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)(\.skill)?\.(md|markdown|txt|zip)$").unwrap());
    static H1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#\s+(.+)").unwrap());
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    if py::char_len(&text) > MAX_FILE {
        return Err(t!("文件太大了，不像是一个技能"));
    }
    let mut head = json!({});
    let mut body = text.as_str();
    if let Some(m) = FRONT.captures(&text) {
        let h = load_yaml(&m[1]).map_err(|e| t!("开头的 YAML 写错了：{e}", e = e))?;
        let h = if py::truthy(&h) { h } else { json!({}) };
        if !h.is_object() {
            return Err(t!("开头的 YAML 应该是一组「键: 值」"));
        }
        head = h;
        body = &text[m.get(0).unwrap().end()..];
    }
    let body = body.trim().to_string();
    let mut name = if py::truthy(&head["name"]) { py::str_of(&head["name"]) } else { String::new() };
    if name.is_empty() && !filename.is_empty() {
        let last = filename.rsplit('/').next().unwrap_or(filename);
        let n = EXT.replace(last, "").into_owned();
        name = if n.to_uppercase() == "SKILL" { String::new() } else { n };
    }
    if name.is_empty()
        && let Some(h) = H1.captures(&body)
    {
        name = h[1].to_string();
    }
    let name = clean_name(&name);
    if name.is_empty() {
        return Err(t!("没找到技能的名字：在文件开头写上 name"));
    }
    let desc = if py::truthy(&head["description"]) { py::str_of(&head["description"]) } else { String::new() };
    check(&name, &body)?;
    let apps = if head.get("apps").is_some_and(|a| !a.is_null()) { json!(clean_apps(&head["apps"])) } else { Value::Null };
    Ok(
        json!({"name": name, "description": py::head(&desc.split_whitespace().collect::<Vec<_>>().join(" "), 200), "body": body, "apps": apps, "files": {}}),
    )
}

fn junk(path: &str) -> bool {
    path.split('/').any(|x| x.starts_with('.') || ["__MACOSX", "Thumbs.db", "__pycache__"].contains(&x))
}

/// 一个技能包（相对路径 → 内容）→ from_file 的结果，加上 files 和 skipped
pub fn package(entries: Vec<(String, Vec<u8>)>, hint: &str) -> Result<Value, String> {
    let entries: Vec<(String, Vec<u8>)> = entries
        .into_iter()
        .map(|(p, v)| (p.replace('\\', "/").trim_start_matches('/').to_string(), v))
        .filter(|(p, _)| !p.is_empty() && !p.ends_with('/') && !junk(p))
        .collect();
    let mut hint = hint.to_string();
    let mut root = String::new();
    if !entries.iter().any(|(p, _)| p.to_lowercase() == "skill.md") {
        let tops: std::collections::HashSet<&str> = entries.iter().map(|(p, _)| p.split('/').next().unwrap_or("")).collect();
        let top = if tops.len() == 1 { tops.into_iter().next().unwrap_or("").to_string() } else { String::new() };
        if top.is_empty() || !entries.iter().any(|(p, _)| p.to_lowercase() == format!("{top}/skill.md").to_lowercase()) {
            return Err(t!("包里没有 SKILL.md：技能包的根上要有一个 SKILL.md"));
        }
        root = format!("{top}/");
        hint = top;
    }
    let mut rel: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (p, v) in entries {
        rel.insert(p.strip_prefix(&root).unwrap_or(&p).to_string(), v);
    }
    let mds: BTreeMap<String, String> = rel.keys().filter(|p| SKILL_MD.is_match(p)).map(|p| (p.to_lowercase(), p.clone())).collect();
    let md = if i18n::lang() == "en" && mds.contains_key("skill.en.md") { mds.get("skill.en.md") } else { mds.get("skill.md") };
    let md = md.ok_or_else(|| t!("包里没有 SKILL.md：技能包的根上要有一个 SKILL.md"))?;
    let text = String::from_utf8(rel[md].clone()).map_err(|_| t!("SKILL.md 不是 UTF-8 文字"))?;
    let mut d = from_file(&text, &hint)?;
    let mut files = Map::new();
    let mut skipped = vec![];
    let md_values: Vec<&String> = mds.values().collect();
    for (p, data) in &rel {
        if md_values.contains(&p) {
            continue;
        }
        let path = clean_path(p);
        let content = String::from_utf8(data.clone()).ok().map(|c| c.trim_start_matches('\u{feff}').to_string());
        match content {
            Some(c) if !path.is_empty() && !c.contains('\0') => {
                files.insert(path, json!(c));
            }
            _ => skipped.push(p.clone()),
        }
    }
    d["files"] = Value::Object(check_files(&Value::Object(files))?);
    d["skipped"] = json!(skipped);
    Ok(d)
}

/// 解析一个 zip 技能包
pub fn from_zip(data: &[u8], filename: &str) -> Result<Value, String> {
    use std::io::Read;
    static ZIPEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.zip$").unwrap());
    if data.len() > MAX_ZIP {
        return Err(t!("压缩包太大了，最多 {n} MB", n = MAX_ZIP / 1_000_000));
    }
    let bad = |e: &str| t!("不是能打开的 zip 文件（{e}）", e = e);
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|_| bad("BadZipFile"))?;
    let mut infos = vec![];
    for i in 0..z.len() {
        let f = z.by_index(i).map_err(|_| bad("BadZipFile"))?;
        if f.is_dir() || junk(f.name()) {
            continue;
        }
        infos.push((i, f.name().to_string(), f.size() as usize));
    }
    if infos.len() > MAX_FILES * 2 {
        return Err(t!("压缩包里的文件太多了，不像是一个技能"));
    }
    let mut entries = vec![];
    let mut total = 0;
    for (i, name, size) in infos {
        total += size;
        if total > MAX_ZIP * 4 {
            return Err(t!("压缩包解开太大了，不像是一个技能"));
        }
        let last = name.rsplit('/').next().unwrap_or(&name);
        let skip = !SKILL_MD.is_match(last) && (BINARY.is_match(&name) || size > MAX_FILE);
        if skip {
            entries.push((name, b"\x00".to_vec()));
            continue;
        }
        let mut f = z.by_index(i).map_err(|_| bad("BadZipFile"))?;
        let mut buf = vec![];
        f.read_to_end(&mut buf).map_err(|_| bad("BadZipFile"))?;
        entries.push((name, buf));
    }
    let base = filename.rsplit('/').next().unwrap_or(filename);
    package(entries, &ZIPEXT.replace(base, ""))
}

pub fn share(sid: i64, on: bool) -> Result<Value, String> {
    let s = get(sid).ok_or_else(|| t!("没有这个技能"))?;
    let token = if on { Some(s["share_token"].as_str().map(String::from).unwrap_or_else(|| util::token_urlsafe(16))) } else { None };
    db::exec("update skills set share_token=? where id=?", params![token, sid]);
    Ok(get(sid).unwrap_or(Value::Null))
}

pub fn by_token(token: &str) -> Option<Value> {
    if token.is_empty() {
        return None;
    }
    db::row(&format!("select {COLS} from skills where share_token=?"), params![token]).map(|r| row_out(&r))
}

fn url_parts(url: &str) -> Option<(String, String, Vec<String>)> {
    let u = reqwest::Url::parse(url.trim()).ok()?;
    let host = u.host_str()?.to_string();
    let netloc = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    };
    let parts = u.path().split('/').filter(|x| !x.is_empty()).map(String::from).collect();
    Some((u.scheme().to_string(), netloc, parts))
}

/// 别人给的链接 → 能直接取到文件内容的地址，和用来猜名字的文件名
pub fn raw_url(url: &str) -> Result<(String, String), String> {
    static MDX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.(md|markdown|txt)$").unwrap());
    let url = url.trim();
    let (scheme, netloc, parts) = url_parts(url)
        .filter(|(s, n, _)| (s == "http" || s == "https") && !n.is_empty())
        .ok_or_else(|| t!("链接要以 http:// 或 https:// 开头"))?;
    if parts.len() == 2 && parts[0] == "skill" {
        return Ok((format!("{scheme}://{netloc}/api/shared/skills/{}/raw", parts[1]), String::new()));
    }
    if netloc == "github.com" && parts.len() >= 5 && (parts[2] == "blob" || parts[2] == "tree") {
        let mut path = parts[4..].join("/");
        let folder;
        if parts[2] == "tree" || !MDX.is_match(&path) {
            folder = parts[parts.len() - 1].clone();
            path += "/SKILL.md";
        } else {
            folder = if parts[parts.len() - 1].to_uppercase() == "SKILL.MD" && parts.len() > 5 {
                parts[parts.len() - 2].clone()
            } else {
                String::new()
            };
        }
        let name = if folder.is_empty() { parts[parts.len() - 1].clone() } else { folder };
        return Ok((format!("https://raw.githubusercontent.com/{}/{}/{}/{path}", parts[0], parts[1], parts[3]), name));
    }
    let name = match parts.last() {
        Some(l) if l.to_uppercase() == "SKILL.MD" && parts.len() > 1 => parts[parts.len() - 2].clone(),
        Some(l) => l.clone(),
        None => String::new(),
    };
    Ok((url.to_string(), name))
}

/// GitHub 上的一个技能文件夹 → (owner, repo, ref, 文件夹路径)
pub fn github_dir(url: &str) -> Option<(String, String, String, String)> {
    static MDX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.(md|markdown|txt)$").unwrap());
    let (_, netloc, parts) = url_parts(url)?;
    if netloc != "github.com" || parts.len() < 5 || (parts[2] != "blob" && parts[2] != "tree") {
        return None;
    }
    let mut path: Vec<String> = parts[4..].to_vec();
    if MDX.is_match(path.last()?) {
        if path.last()?.to_uppercase() != "SKILL.MD" {
            return None;
        }
        path.pop();
    }
    Some((parts[0].clone(), parts[1].clone(), parts[3].clone(), path.join("/")))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(20))
        .user_agent("python-httpx/0.28.1")
        .build()
        .unwrap_or_default()
}

fn http_err(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "ReadTimeout"
    } else if e.is_connect() {
        "ConnectError"
    } else {
        "HTTPError"
    };
    t!("取不到这个链接（{e}）", e = kind)
}

async fn fetch_bytes(c: &reqwest::Client, url: &str, limit: usize, accept: &str) -> Result<Vec<u8>, String> {
    use futures::StreamExt;
    let r = c.get(url).header("accept", accept).send().await.map_err(|e| http_err(&e))?;
    let code = r.status().as_u16();
    if code == 404 {
        return Err(t!("链接打不开（404）：分享可能已经取消了，或者地址不对"));
    }
    if code >= 400 {
        return Err(t!("链接打不开（{code}）", code = code));
    }
    if r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").contains("text/html") {
        return Err(t!("这个链接打开是一个网页，不是技能文件。GitHub 上的请用文件或文件夹的链接"));
    }
    let mut data = vec![];
    let mut s = r.bytes_stream();
    while let Some(chunk) = s.next().await {
        data.extend_from_slice(&chunk.map_err(|e| http_err(&e))?);
        if data.len() > limit {
            return Err(t!("文件太大了，不像是一个技能"));
        }
    }
    Ok(data)
}

fn url_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn github_files(c: &reqwest::Client, owner: &str, repo: &str, r#ref: &str, folder: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut found: Vec<(String, Value)> = vec![];
    let mut stack: Vec<(String, String, usize)> = vec![(folder.to_string(), String::new(), 0)];
    // 和 Python 版一样深度优先、按列表顺序
    async fn walk(
        c: &reqwest::Client,
        owner: &str,
        repo: &str,
        r#ref: &str,
        path: &str,
        rel: &str,
        depth: usize,
        found: &mut Vec<(String, Value)>,
    ) -> Result<(), String> {
        let r = c
            .get(format!("https://api.github.com/repos/{owner}/{repo}/contents/{}", url_quote(path)))
            .query(&[("ref", r#ref)])
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| http_err(&e))?;
        let code = r.status().as_u16();
        if code == 403 || code == 429 {
            return Err(t!("GitHub 限制了没登录时的访问次数，过一会儿再试"));
        }
        if code >= 400 {
            return Err(t!("列不出 GitHub 上这个文件夹里的文件（{code}）", code = code));
        }
        let items: Value = r.json().await.unwrap_or(Value::Null);
        for it in items.as_array().cloned().unwrap_or_default() {
            let sub = format!("{rel}{}", py::str_of(&it["name"]));
            if junk(&sub) {
                continue;
            }
            if it["type"] == "dir" && depth < 3 {
                let p = it["path"].as_str().map(String::from).unwrap_or_else(|| format!("{path}/{}", py::str_of(&it["name"])));
                Box::pin(walk(c, owner, repo, r#ref, &p, &format!("{sub}/"), depth + 1, found)).await?;
            } else if it["type"] == "file" {
                found.push((sub, it.clone()));
            }
            if found.len() > MAX_FILES * 2 {
                return Err(t!("这个文件夹里的文件太多了，不像是一个技能"));
            }
        }
        Ok(())
    }
    if let Some((p, rel, d)) = stack.pop() {
        walk(c, owner, repo, r#ref, &p, &rel, d, &mut found).await?;
    }
    let futs = found.iter().map(|(sub, it)| async move {
        let url = it["download_url"].as_str().unwrap_or("");
        let size = it["size"].as_u64().unwrap_or(0) as usize;
        if url.is_empty() || (!SKILL_MD.is_match(sub) && (BINARY.is_match(sub) || size > MAX_FILE)) {
            return Ok(b"\x00".to_vec());
        }
        fetch_bytes(c, url, MAX_FILE, "*/*").await
    });
    let data = futures::future::join_all(futs).await;
    let mut out = vec![];
    for ((sub, _), d) in found.into_iter().zip(data) {
        out.push((sub, d?));
    }
    Ok(out)
}

/// 从链接取一个技能（.md 或 zip；GitHub 上的文件夹连里面的文件一起取）并解析
pub async fn fetch(url: &str) -> Result<Value, String> {
    let (src, name) = raw_url(url)?;
    let gh = github_dir(url);
    let mut warning = String::new();
    let c = client();
    if let Some((o, r, rf, f)) = gh {
        match github_files(&c, &o, &r, &rf, &f).await {
            Ok(entries) => return package(entries, &name),
            Err(e) => warning = t!("只取到了 SKILL.md，文件夹里的其他文件没取到（{e}）", e = e),
        }
    }
    let data = fetch_bytes(&c, &src, MAX_ZIP, "text/markdown, text/plain;q=0.9, */*;q=0.5").await?;
    if data.starts_with(b"PK\x03\x04") {
        return from_zip(&data, if name.is_empty() { src.rsplit('/').next().unwrap_or("") } else { &name });
    }
    if data.len() > MAX_FILE {
        return Err(t!("文件太大了，不像是一个技能"));
    }
    let text = String::from_utf8(data).map_err(|_| t!("不是文字文件"))?;
    let mut d = from_file(&text, &name)?;
    d["skipped"] = json!([]);
    if !warning.is_empty() {
        d["warning"] = json!(warning);
    }
    Ok(d)
}

// ---------- 技能市场 ----------
/// 编进二进制的技能市场和内置 Bot 模板（assets/skills.json）
pub mod bundled {
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::sync::LazyLock;

    static DATA: LazyLock<Value> = LazyLock::new(|| serde_json::from_str(include_str!("../assets/skills.json")).expect("skills.json"));
    static MARKET: LazyLock<BTreeMap<String, BTreeMap<String, String>>> = LazyLock::new(|| {
        DATA["market"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.as_object()
                                .map(|f| f.iter().map(|(p, t)| (p.clone(), t.as_str().unwrap_or("").to_string())).collect())
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    });

    pub fn market() -> &'static BTreeMap<String, BTreeMap<String, String>> {
        &MARKET
    }

    pub fn templates() -> BTreeMap<String, String> {
        DATA["templates"]
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default()
    }
}

pub fn market_skill(mid: &str) -> Result<Value, String> {
    let files = bundled::market().get(mid).cloned();
    let files = files.filter(|f| MARKET_ID.is_match(mid) && f.contains_key("SKILL.md")).ok_or_else(|| t!("技能市场里没有这个技能"))?;
    package(files.into_iter().map(|(k, v)| (k, v.into_bytes())).collect(), mid)
}

pub fn market(bot_id: Option<i64>) -> Vec<Value> {
    let mut out = vec![];
    for (id, files) in bundled::market() {
        if !files.contains_key("SKILL.md") {
            continue;
        }
        let s = match market_skill(id) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("技能市场的 {id} 有问题，跳过：{e}");
                continue;
            }
        };
        let have = bot_id.and_then(|b| find(b, s["name"].as_str().unwrap_or("")));
        out.push(json!({"id": id, "name": s["name"], "description": s["description"], "body": s["body"],
            "files": file_names(&s), "apps": apps_of(&s), "installed": have.is_some()}));
    }
    out
}

pub fn install(bot_id: Option<i64>, mid: &str) -> Result<Value, String> {
    let s = market_skill(mid)?;
    save_full(
        bot_id,
        s["name"].as_str().unwrap_or(""),
        s["body"].as_str().unwrap_or(""),
        s["description"].as_str().unwrap_or(""),
        "market",
        None,
        Some(&s["files"]),
        Apps::Set(s["apps"].clone()),
    )
}

// ---------- 接口 ----------
fn bot_or_404(bot_id: i64) -> Result<Value, ApiError> {
    bots::get(bot_id).ok_or_else(|| ApiError::not_found(t!("没有这个 Bot")))
}

async fn list_skills(Path(bot_id): Path<i64>) -> Res {
    bot_or_404(bot_id)?;
    ok(json!(list_for(bot_id)))
}

async fn create_skill(Path(bot_id): Path<i64>, Body(d): Body) -> Res {
    bot_or_404(bot_id)?;
    let owner = if py::truthy(&d["shared"]) { None } else { Some(bot_id) };
    let source = d["source"].as_str().unwrap_or("manual");
    let files = if py::truthy(&d["files"]) { d["files"].clone() } else { json!({}) };
    let s = save_full(
        owner,
        d["name"].as_str().unwrap_or(""),
        d["body"].as_str().unwrap_or(""),
        d["description"].as_str().unwrap_or(""),
        source,
        None,
        Some(&files),
        Apps::Set(d["apps"].clone()),
    )?;
    broadcast(json!({"type": "skills_changed"}));
    ok(s)
}

async fn skill_market(Path(bot_id): Path<i64>) -> Res {
    bot_or_404(bot_id)?;
    ok(json!(market(Some(bot_id))))
}

async fn install_market(Path((bot_id, mid)): Path<(i64, String)>) -> Res {
    bot_or_404(bot_id)?;
    let s = install(Some(bot_id), &mid)?;
    broadcast(json!({"type": "skills_changed"}));
    ok(s)
}

async fn update_skill(Path(sid): Path<i64>, Body(d): Body) -> Res {
    let old = get(sid).ok_or_else(|| ApiError::not_found(t!("没有这个技能")))?;
    let owner = if d.get("shared").is_none() {
        old["bot_id"].as_i64()
    } else if py::truthy(&d["shared"]) {
        None
    } else {
        match d.get("bot_id") {
            Some(b) => b.as_i64(),
            None => old["bot_id"].as_i64(),
        }
    };
    let pick = |k: &str| -> String {
        d.get(k).and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| old[k].as_str().unwrap_or("").to_string())
    };
    let files = d.get("files").map(|f| if py::truthy(f) { f.clone() } else { json!({}) });
    let apps = match d.get("apps") {
        Some(a) => Apps::Set(a.clone()),
        None => Apps::Keep,
    };
    let s = save_full(
        owner,
        &pick("name"),
        &pick("body"),
        &pick("description"),
        old["source"].as_str().unwrap_or("manual"),
        Some(sid),
        files.as_ref(),
        apps,
    )?;
    broadcast(json!({"type": "skills_changed"}));
    ok(s)
}

async fn delete_skill(Path(sid): Path<i64>) -> Res {
    delete(sid);
    broadcast(json!({"type": "skills_changed"}));
    ok(json!({"ok": true}))
}

pub fn quote_name(s: &str) -> String {
    url_quote(s)
}

fn skill_file(s: &Value) -> Response {
    let (data, name, kind) = export(s);
    (
        [
            (header::CONTENT_TYPE, kind.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename*=UTF-8''{}", url_quote(&name))),
        ],
        data,
    )
        .into_response()
}

async fn export_skill(Path(sid): Path<i64>) -> Response {
    match get(sid) {
        Some(s) => skill_file(&s),
        None => ApiError::not_found(t!("没有这个技能")).into_response(),
    }
}

async fn share_skill(Path(sid): Path<i64>, Body(d): Body) -> Res {
    let s = share(sid, py::truthy(&d["on"])).map_err(ApiError::not_found)?;
    broadcast(json!({"type": "skills_changed"}));
    ok(s)
}

/// 技能要用的应用连没连上
pub fn app_status(names: &[String]) -> Vec<Value> {
    let have = mcp_hub::all_apps();
    let known = catalog::items();
    names
        .iter()
        .map(|a| {
            let k = known.contains_key(a);
            let h = have.contains(a);
            json!({"name": a, "title": if k || h { catalog::title(a) } else { format!("@{a}") }, "connected": h, "in_market": k})
        })
        .collect()
}

async fn parse_skill(Path(bot_id): Path<i64>, Body(data): Body) -> Res {
    bot_or_404(bot_id)?;
    let filename = data["filename"].as_str().unwrap_or("");
    let mut d = if !data["url"].as_str().unwrap_or("").trim().is_empty() {
        fetch(data["url"].as_str().unwrap_or("")).await?
    } else if py::truthy(&data["zip"]) {
        let raw = util::b64_decode(data["zip"].as_str().unwrap_or(""))
            .ok_or_else(|| ApiError::bad(t!("不是能打开的 zip 文件（{e}）", e = "base64")))?;
        from_zip(&raw, filename)?
    } else {
        let mut d = from_file(data["text"].as_str().unwrap_or(""), filename)?;
        d["skipped"] = json!([]);
        d
    };
    let declared = d.as_object_mut().and_then(|o| o.remove("apps")).unwrap_or(Value::Null);
    let mut probe = d.clone();
    probe["apps"] = declared.clone();
    d["declared_apps"] = declared;
    d["apps"] = json!(app_status(&apps_of(&probe)));
    d["exists"] = json!(find(bot_id, d["name"].as_str().unwrap_or("")).is_some());
    ok(d)
}

async fn shared_skill(Path(token): Path<String>) -> Res {
    let s = by_token(&token).ok_or_else(|| ApiError::not_found(t!("链接已失效")))?;
    let mut out = json!({"name": s["name"], "description": s["description"], "body": s["body"], "updated": s["updated"],
        "apps": apps_of(&s), "files": file_names(&s)});
    if let Some(b) = s["bot_id"].as_i64().and_then(bots::get) {
        out["bot"] = json!({"name": b["name"], "avatar": b["avatar"]});
    }
    ok(out)
}

async fn shared_skill_raw(Path(token): Path<String>) -> Response {
    match by_token(&token) {
        Some(s) => skill_file(&s),
        None => ApiError::not_found(t!("链接已失效")).into_response(),
    }
}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/bots/{bot_id}/skills", route_get(list_skills).post(create_skill))
        .route("/api/bots/{bot_id}/skills/market", route_get(skill_market))
        .route("/api/bots/{bot_id}/skills/market/{mid}", post(install_market))
        .route("/api/bots/{bot_id}/skills/parse", post(parse_skill))
        .route("/api/skills/{sid}", axum::routing::put(update_skill).delete(delete_skill))
        .route("/api/skills/{sid}/export", route_get(export_skill))
        .route("/api/skills/{sid}/share", post(share_skill))
        .route("/api/shared/skills/{token}", route_get(shared_skill))
        .route("/api/shared/skills/{token}/raw", route_get(shared_skill_raw))
}

#[allow(dead_code)]
fn _unused(_: i64) -> Result<i64, ApiError> {
    int(&json!(0))
}
