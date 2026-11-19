//! 应用市场的目录。对应 osa/catalog/__init__.py（格式见 docs/apps.md）。
//!
//! 每个应用一份 YAML 清单，文件名就是应用名：随项目发布的编进二进制（files.rs，server/scripts/dump_catalog.py 生成）；
//! 数据目录下 apps/<名字>.yaml 只在这台机器上用，和内置的同名时覆盖内置的，改了不用重启。
//! en 是英文界面用的文案，读清单时原样留着，到 public()、title() 这些地方再按当前语言换（en_of）。

pub mod checks;
mod files;

use crate::{config, i18n, py, vault};
use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::LazyLock;

pub const CATEGORIES: [(&str, &str); 8] = [
    ("notes", "笔记与文档"),
    ("mail", "邮件与日历"),
    ("files", "文件"),
    ("tasks", "任务与协作"),
    ("design", "设计"),
    ("dev", "开发"),
    ("web", "搜索与网页"),
    ("life", "生活"),
];
const GLYPHS: [&str; 19] = [
    "globe", "folder", "mail", "search", "train", "pin", "book", "doc", "feather", "plug", "chat", "hash", "package", "cloud", "palette", "table",
    "calendar", "check", "notebook",
];
const KEYS: [&str; 21] = [
    "title", "category", "order", "by", "icon", "desc", "about", "can", "example", "note", "how", "steps", "link", "link_label", "copy", "fields",
    "check", "deny", "confirm", "connect", "en",
];
const EN_TEXT: [&str; 8] = ["title", "by", "desc", "about", "example", "note", "how", "link_label"];
const EN_LIST: [&str; 2] = ["can", "steps"];
const EN_FIELD_KEYS: [&str; 3] = ["label", "placeholder", "invalid"];
const FIELD_KEYS: [&str; 9] = ["key", "label", "placeholder", "default", "secret", "optional", "nospace", "pattern", "invalid"];
const CONNECT_KEYS: [&str; 7] = ["type", "command", "args", "env", "url", "headers", "oauth"];
const OAUTH_KEYS: [&str; 4] = ["client_id", "client_secret", "scope", "params"];

fn en_keys() -> Vec<&'static str> {
    let mut v: Vec<&str> = EN_TEXT.iter().chain(EN_LIST.iter()).copied().collect();
    v.extend(["copy", "fields", "check"]);
    v
}

static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]{0,23}$").unwrap());
static MENTION: LazyLock<fancy_regex::Regex> = LazyLock::new(|| fancy_regex::Regex::new(r"(?<![\w@.])@([a-z][a-z0-9_-]{0,23})\b").unwrap());
static SLOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{(\w+)\}").unwrap());
static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(env|vault):|\{(env|vault):").unwrap());
static COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#[0-9A-Fa-f]{6}$").unwrap());
static SVG_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[MmZzLlHhVvCcSsQqTtAa0-9eE.,+\-\s]+$").unwrap());
static FIELD_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]*$").unwrap());
static URL_OK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(https?://|\{\w+\}$)").unwrap());

pub fn local_dir() -> PathBuf {
    config::DATA.join("apps")
}

pub fn workspace() -> PathBuf {
    config::DATA.join("workspace")
}

fn parse_yaml(text: &str) -> Result<Value, String> {
    serde_yaml::from_str::<serde_yaml::Value>(text).map(config::yaml_to_json).map_err(|e| match e.location() {
        Some(l) => format!("第 {} 行 YAML 格式不对（{e}）", l.line()),
        None => format!("YAML 格式不对（{e}）"),
    })
}

struct Cache {
    sig: Option<Vec<(String, i128)>>,
    items: IndexMap<String, Value>,
    errors: Vec<String>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(Cache { sig: None, items: IndexMap::new(), errors: vec![] }));

fn local_files() -> Vec<(String, PathBuf, i128)> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(local_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("yaml") {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let mt = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos() as i128).unwrap_or(0);
            out.push((stem, p, mt));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn show(p: &std::path::Path) -> String {
    p.strip_prefix(&*config::ROOT).map(|r| r.display().to_string()).unwrap_or_else(|_| p.display().to_string())
}

/// 读所有清单：(应用名 -> 清单, 有错的清单和原因)。本机的文件都没变就用上次读的
pub fn load() -> (IndexMap<String, Value>, Vec<String>) {
    let locals = local_files();
    let sig: Vec<(String, i128)> = locals.iter().map(|(_, p, m)| (p.display().to_string(), *m)).collect();
    let mut c = CACHE.lock();
    if c.sig.as_ref() != Some(&sig) {
        let mut files: IndexMap<String, (String, Result<String, String>)> = IndexMap::new();
        for (name, text) in files::BUILTIN {
            files.insert(name.to_string(), (format!("osa/catalog/apps/{name}.yaml"), Ok(text.to_string())));
        }
        for (name, p, _) in &locals {
            files.insert(name.clone(), (show(p), std::fs::read_to_string(p).map_err(|e| e.to_string())));
        }
        let mut items: Vec<(String, Value)> = vec![];
        let mut errors = vec![];
        for (name, (shown, text)) in files {
            match text.and_then(|t| parse_yaml(&t)).and_then(|m| parse(&name, &m)) {
                Ok(it) => items.push((name, it)),
                Err(e) => errors.push(format!("{shown}：{e}")),
            }
        }
        let rank = |c: &str| CATEGORIES.iter().position(|(k, _)| *k == c).unwrap_or(99);
        items.sort_by(|a, b| {
            (rank(a.1["category"].as_str().unwrap_or("")), a.1["order"].as_i64().unwrap_or(100), a.0.clone()).cmp(&(
                rank(b.1["category"].as_str().unwrap_or("")),
                b.1["order"].as_i64().unwrap_or(100),
                b.0.clone(),
            ))
        });
        for e in &errors {
            tracing::warn!("应用清单有错，已跳过：{e}");
        }
        *c = Cache { sig: Some(sig), items: items.into_iter().collect(), errors };
    }
    (c.items.clone(), c.errors.clone())
}

pub fn items() -> IndexMap<String, Value> {
    load().0
}

static BUILTIN_CACHE: LazyLock<Mutex<HashMap<String, Option<Value>>>> = LazyLock::new(Default::default);

/// 随项目发布的清单（不看本机加的、覆盖的）。服务器锁只认这里的启动命令（见 security.rs）
pub fn builtin(name: &str) -> Option<Value> {
    if !ID.is_match(name) {
        return None;
    }
    if let Some(hit) = BUILTIN_CACHE.lock().get(name) {
        return hit.clone();
    }
    let it = files::BUILTIN.iter().find(|(n, _)| *n == name).and_then(|(_, text)| parse_yaml(text).ok()).and_then(|m| parse(name, &m).ok());
    BUILTIN_CACHE.lock().insert(name.to_string(), it.clone());
    it
}

fn is_text(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.trim().is_empty())
}

/// 检查一份清单并补上默认值。有问题时一次列出所有问题
pub fn parse(name: &str, m: &Value) -> Result<Value, String> {
    let Some(mo) = m.as_object() else { return Err("内容应该是一组「键: 值」".into()) };
    let mut bad: Vec<String> = vec![];
    if !ID.is_match(name) {
        bad.push("文件名就是应用名：小写字母开头，只能有小写字母、数字、- 和 _，最长 24 个字符".into());
    }
    for k in mo.keys() {
        if !KEYS.contains(&k.as_str()) {
            bad.push(format!("不认识的键 {k}"));
        }
    }
    let text = |bad: &mut Vec<String>, v: Option<&Value>, where_: &str, need: bool| match v {
        None | Some(Value::Null) => {
            if need {
                bad.push(format!("缺少 {where_}"));
            }
        }
        Some(v) => {
            if !is_text(v) {
                bad.push(format!("{where_} 应该是一段文字"));
            }
        }
    };
    let texts = |bad: &mut Vec<String>, v: Option<&Value>, where_: &str| {
        if let Some(v) = v.filter(|v| !v.is_null()) {
            if !v.as_array().is_some_and(|a| a.iter().all(is_text)) {
                bad.push(format!("{where_} 应该是一组文字"));
            }
        }
    };
    for k in ["title", "by", "desc"] {
        text(&mut bad, mo.get(k), k, true);
    }
    for k in ["about", "example", "note", "how", "link", "link_label"] {
        text(&mut bad, mo.get(k), k, false);
    }
    for k in ["can", "steps", "deny"] {
        texts(&mut bad, mo.get(k), k);
    }
    let cat = mo.get("category").and_then(|v| v.as_str()).unwrap_or("");
    if !CATEGORIES.iter().any(|(k, _)| *k == cat) {
        bad.push(format!("category 应该是 {} 之一", CATEGORIES.iter().map(|(k, _)| *k).collect::<Vec<_>>().join("、")));
    }
    if let Some(o) = mo.get("order") {
        if !o.is_i64() && !o.is_u64() {
            bad.push("order 应该是一个整数".into());
        }
    }
    let cf = mo.get("confirm").cloned().unwrap_or(json!("auto"));
    let cf_ok = matches!(cf.as_str(), Some("auto") | Some("all") | Some("none"))
        || cf.as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|x| x.is_string()));
    if !cf_ok {
        bad.push("confirm 应该是 auto、all、none，或者一组词（工具名里含这些词就先问）".into());
    }
    check_icon(mo.get("icon"), &mut bad);
    if let Some(cp) = mo.get("copy").filter(|v| !v.is_null()) {
        let ok = cp.as_object().is_some_and(|o| {
            let ks: BTreeSet<&str> = o.keys().map(|s| s.as_str()).collect();
            ks == BTreeSet::from(["label", "text"]) && o.values().all(is_text)
        });
        if !ok {
            bad.push("copy 要写 label 和 text 两项文字".into());
        }
    }
    let fields = check_fields(mo.get("fields"), &mut bad);
    let keys: BTreeSet<String> = fields.iter().filter_map(|f| f["key"].as_str().map(String::from)).collect();
    let mut used = check_connect(mo.get("connect"), &keys, &mut bad);
    used.extend(check_checks(mo.get("check"), &keys, &mut bad));
    for k in keys.difference(&used) {
        bad.push(format!("字段 {k} 没有用在 connect 或 check 里"));
    }
    check_en(mo.get("en"), mo, &fields, &mut bad, &text, &texts);
    let o = mo.get("connect").and_then(|c| c.get("oauth")).cloned().unwrap_or(Value::Null);
    let oauth = o == Value::Bool(true) || o.is_object();
    if !bad.is_empty() {
        return Err(bad.join("；"));
    }
    let builtin_subst: HashMap<String, String> = HashMap::from([("workspace".to_string(), workspace().display().to_string())]);
    let mut out = mo.clone();
    let or = |k: &str, d: Value| mo.get(k).filter(|v| py::truthy(v)).cloned().unwrap_or(d);
    out.insert("id".into(), json!(name));
    out.insert("order".into(), mo.get("order").cloned().unwrap_or(json!(100)));
    out.insert("confirm".into(), mo.get("confirm").cloned().unwrap_or(json!("auto")));
    let kind = if oauth { "oauth" } else if fields.iter().any(|f| py::truthy(&f["secret"])) { "token" } else { "none" };
    out.insert("kind".into(), json!(kind));
    out.insert("client".into(), json!(o.is_object() && o.get("client_id").is_some()));
    out.insert("can".into(), or("can", json!([])));
    out.insert("steps".into(), or("steps", json!([])));
    out.insert("deny".into(), or("deny", json!([])));
    out.insert("check".into(), or("check", json!({})));
    let fields: Vec<Value> = fields
        .into_iter()
        .map(|mut f| {
            if let Some(d) = f.get("default").cloned() {
                f["default"] = fill(&d, &builtin_subst);
            }
            f
        })
        .collect();
    out.insert("fields".into(), json!(fields));
    Ok(Value::Object(out))
}

type TextFn<'a> = &'a dyn Fn(&mut Vec<String>, Option<&Value>, &str, bool);
type TextsFn<'a> = &'a dyn Fn(&mut Vec<String>, Option<&Value>, &str);

fn mentions(s: &str) -> BTreeSet<String> {
    MENTION.captures_iter(s).filter_map(|c| c.ok()).filter_map(|c| c.get(1).map(|x| x.as_str().to_string())).collect()
}

fn check_en(en: Option<&Value>, m: &Map<String, Value>, fields: &[Value], bad: &mut Vec<String>, text: TextFn, texts: TextsFn) {
    let Some(en) = en.filter(|v| !v.is_null()) else { return };
    let Some(eo) = en.as_object() else {
        bad.push("en 应该是一组「键: 值」（英文界面用的文案）".into());
        return;
    };
    let ek = en_keys();
    let mut sorted = ek.clone();
    sorted.sort();
    for k in eo.keys() {
        if !ek.contains(&k.as_str()) {
            bad.push(format!("en 里不认识的键 {k}（能写的：{}）", sorted.join("、")));
        }
    }
    for k in eo.keys() {
        if ek.contains(&k.as_str()) && !m.contains_key(k) {
            bad.push(format!("en.{k} 没有对应的中文 {k}"));
        }
    }
    for k in EN_TEXT {
        text(bad, eo.get(k), &format!("en.{k}"), false);
    }
    for k in EN_LIST {
        texts(bad, eo.get(k), &format!("en.{k}"));
    }
    if let (Some(e), Some(z)) = (eo.get("example").and_then(|v| v.as_str()), m.get("example").and_then(|v| v.as_str())) {
        let (want, got) = (mentions(z), mentions(e));
        if got != want {
            let w: Vec<String> = want.iter().map(|x| format!("@{x}")).collect();
            bad.push(format!("en.example 要和中文的 example 一样 @ 这些应用：{}", if w.is_empty() { "（没有）".into() } else { w.join("、") }));
        }
    }
    if let Some(cp) = eo.get("copy").filter(|v| !v.is_null()) {
        let ok = cp.as_object().is_some_and(|o| o.len() == 1 && o.get("label").is_some_and(is_text));
        if !ok {
            bad.push("en.copy 只写 label 一项文字（要复制的内容中英文都一样）".into());
        }
    }
    if let Some(ef) = eo.get("fields").filter(|v| !v.is_null()) {
        match ef.as_object() {
            None => bad.push("en.fields 应该写成「字段的 key: {label: …}」".into()),
            Some(efo) => {
                let have: HashMap<String, &Value> = fields.iter().filter_map(|f| f["key"].as_str().map(|k| (k.to_string(), f))).collect();
                for (k, v) in efo {
                    let Some(hf) = have.get(k) else {
                        bad.push(format!("en.fields 里的 {k} 不是 fields 里的字段"));
                        continue;
                    };
                    let Some(vo) = v.as_object() else {
                        bad.push(format!("en.fields.{k} 应该写成 label、placeholder、invalid 这样的一组键"));
                        continue;
                    };
                    for x in vo.keys() {
                        if !EN_FIELD_KEYS.contains(&x.as_str()) {
                            bad.push(format!("en.fields.{k} 里不认识的键 {x}（能写的：{}）", EN_FIELD_KEYS.join("、")));
                        }
                    }
                    for x in vo.keys() {
                        if EN_FIELD_KEYS.contains(&x.as_str()) && hf.get(x).is_none() {
                            bad.push(format!("en.fields.{k}.{x} 没有对应的中文 {x}"));
                        }
                    }
                    for x in EN_FIELD_KEYS {
                        text(bad, vo.get(x), &format!("en.fields.{k}.{x}"), false);
                    }
                }
            }
        }
    }
    if let Some(ec) = eo.get("check").filter(|v| !v.is_null()) {
        let ck = m.get("check").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        match ec.as_object() {
            None => bad.push("en.check 应该写成「检查名: {hint: …}」".into()),
            Some(eco) => {
                for (k, v) in eco {
                    if !ck.contains_key(k) {
                        bad.push(format!("en.check 里的 {k} 不是 check 里的检查"));
                    } else if !v.as_object().is_some_and(|o| o.len() == 1 && o.contains_key("hint")) {
                        bad.push(format!("en.check.{k} 只写 hint 一项"));
                    } else if !ck[k].as_object().is_some_and(|o| o.contains_key("hint")) {
                        bad.push(format!("en.check.{k}.hint 没有对应的中文 hint"));
                    } else {
                        text(bad, v.get("hint"), &format!("en.check.{k}.hint"), false);
                    }
                }
            }
        }
    }
}

fn check_icon(ic: Option<&Value>, bad: &mut Vec<String>) {
    let Some(ic) = ic.filter(|v| !v.is_null()) else { return };
    let Some(o) = ic.as_object() else {
        bad.push("icon 应该写 color 和 svg（或 glyph）".into());
        return;
    };
    for k in o.keys() {
        if !["color", "svg", "glyph"].contains(&k.as_str()) {
            bad.push(format!("icon 里不认识的键 {k}"));
        }
    }
    let color = o.get("color").filter(|v| py::truthy(v)).map(py::str_of).unwrap_or_default();
    if !COLOR.is_match(&color) {
        bad.push("icon.color 应该是 #RRGGBB 格式的颜色".into());
    }
    if o.contains_key("svg") && o.contains_key("glyph") {
        bad.push("icon 里 svg 和 glyph 只写一个".into());
    }
    if let Some(s) = o.get("svg") {
        if !s.as_str().is_some_and(|s| SVG_PATH.is_match(s)) {
            bad.push("icon.svg 应该是 24×24 的 SVG path（<path d=\"…\"> 里 d 的值）".into());
        }
    }
    if let Some(g) = o.get("glyph") {
        if !g.as_str().is_some_and(|g| GLYPHS.contains(&g)) {
            let mut gs: Vec<&str> = GLYPHS.to_vec();
            gs.sort();
            bad.push(format!("icon.glyph 应该是 {} 之一", gs.join("、")));
        }
    }
}

fn check_fields(fields: Option<&Value>, bad: &mut Vec<String>) -> Vec<Value> {
    let Some(fields) = fields.filter(|v| !v.is_null()) else { return vec![] };
    let Some(arr) = fields.as_array() else {
        bad.push("fields 应该是一组字段".into());
        return vec![];
    };
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    for (i, f) in arr.iter().enumerate() {
        let where_ = format!("fields 第 {} 项", i + 1);
        let Some(fo) = f.as_object() else {
            bad.push(format!("{where_} 应该是一组「键: 值」"));
            continue;
        };
        for k in fo.keys() {
            if !FIELD_KEYS.contains(&k.as_str()) {
                bad.push(format!("{where_}：不认识的键 {k}"));
            }
        }
        let Some(k) = fo.get("key").and_then(|v| v.as_str()).filter(|k| FIELD_KEY.is_match(k)) else {
            bad.push(format!("{where_}：key 只能用小写字母、数字和 _，字母开头"));
            continue;
        };
        if !seen.insert(k.to_string()) {
            bad.push(format!("{where_}：key {k} 重复了"));
        }
        if !fo.get("label").is_some_and(is_text) {
            bad.push(format!("{where_}：缺少 label（给用户看的名字）"));
        }
        for b in ["secret", "optional", "nospace"] {
            if fo.get(b).is_some_and(|v| !v.is_boolean()) {
                bad.push(format!("{where_}：{b} 应该是 true 或 false"));
            }
        }
        for s in ["placeholder", "default", "pattern", "invalid"] {
            if fo.get(s).is_some_and(|v| !v.is_string()) {
                bad.push(format!("{where_}：{s} 应该是文字（数字请加引号）"));
            }
        }
        if let Some(p) = fo.get("pattern").and_then(|v| v.as_str()) {
            if let Err(e) = fancy_regex::Regex::new(p) {
                bad.push(format!("{where_}：pattern 不是有效的正则（{e}）"));
            }
        }
        out.push(f.clone());
    }
    out
}

fn slots(v: &Value) -> BTreeSet<String> {
    SLOT.captures_iter(&py::dumps_u(v)).map(|c| c[1].to_string()).collect()
}

fn check_connect(c: Option<&Value>, keys: &BTreeSet<String>, bad: &mut Vec<String>) -> BTreeSet<String> {
    let Some(co) = c.and_then(|c| c.as_object()) else {
        bad.push("缺少 connect（怎么连这个应用）".into());
        return BTreeSet::new();
    };
    for k in co.keys() {
        if !CONNECT_KEYS.contains(&k.as_str()) {
            bad.push(format!("connect 里不认识的键 {k}"));
        }
    }
    let strmap = |bad: &mut Vec<String>, k: &str| {
        if let Some(v) = co.get(k).filter(|v| !v.is_null()) {
            if !v.as_object().is_some_and(|o| o.values().all(|x| x.is_string())) {
                bad.push(format!("connect.{k} 的值都要是文字（数字请加引号，比如 '993'）"));
            }
        }
    };
    let t = co.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if t == "stdio" {
        if !co.get("command").is_some_and(is_text) {
            bad.push("本机运行的应用（type: stdio）要写 command".into());
        }
        if let Some(a) = co.get("args") {
            if !a.as_array().is_some_and(|a| a.iter().all(|x| x.is_string())) {
                bad.push("connect.args 应该是一组文字（数字请加引号）".into());
            }
        }
        strmap(bad, "env");
        for k in ["url", "headers", "oauth"] {
            if co.contains_key(k) {
                bad.push(format!("本机运行的应用不能写 {k}"));
            }
        }
    } else if t == "http" || t == "sse" {
        if !co.get("url").and_then(|v| v.as_str()).is_some_and(|u| URL_OK.is_match(u)) {
            bad.push(format!("远程应用（type: {t}）要写 url，以 https:// 开头"));
        }
        strmap(bad, "headers");
        check_oauth(co.get("oauth"), bad);
        for k in ["command", "args", "env"] {
            if co.contains_key(k) {
                bad.push(format!("远程应用不能写 {k}"));
            }
        }
    } else {
        bad.push("connect.type 应该是 stdio（在本机运行）、http（远程地址）或 sse（只有旧版 SSE 地址的远程应用）".into());
    }
    let used = slots(&Value::Object(co.clone()));
    for s in used.difference(keys) {
        bad.push(format!("connect 里的 {{{s}}} 不是 fields 里的字段"));
    }
    used
}

fn check_oauth(o: Option<&Value>, bad: &mut Vec<String>) {
    let Some(o) = o.filter(|v| !v.is_null() && !v.is_boolean()) else { return };
    let Some(oo) = o.as_object() else {
        bad.push("connect.oauth 应该是 true，或者写成 client_id、scope 这样的一组键".into());
        return;
    };
    for k in oo.keys() {
        if !OAUTH_KEYS.contains(&k.as_str()) {
            bad.push(format!("connect.oauth 里不认识的键 {k}"));
        }
    }
    for k in ["client_id", "client_secret", "scope"] {
        if oo.get(k).is_some_and(|v| !is_text(v)) {
            bad.push(format!("connect.oauth.{k} 应该是一段文字"));
        }
    }
    if oo.contains_key("client_secret") && !oo.contains_key("client_id") {
        bad.push("connect.oauth 写了 client_secret 就要写 client_id".into());
    }
    if let Some(p) = oo.get("params").filter(|v| !v.is_null()) {
        if !p.as_object().is_some_and(|o| o.values().all(|x| x.is_string())) {
            bad.push("connect.oauth.params 的值都要是文字".into());
        }
    }
}

fn check_checks(ck: Option<&Value>, keys: &BTreeSet<String>, bad: &mut Vec<String>) -> BTreeSet<String> {
    let Some(ck) = ck.filter(|v| !v.is_null()) else { return BTreeSet::new() };
    let Some(co) = ck.as_object() else {
        bad.push("check 应该写成「检查名: 参数」".into());
        return BTreeSet::new();
    };
    let mut used = BTreeSet::new();
    for (name, params) in co {
        let Some((required, all)) = checks::signature(name) else {
            bad.push(format!("没有叫 {name} 的检查，现有的：{}", checks::NAMES.join("、")));
            continue;
        };
        let Some(po) = params.as_object() else {
            bad.push(format!("check.{name} 应该写成一组参数"));
            continue;
        };
        for p in required {
            if !po.contains_key(*p) {
                bad.push(format!("check.{name} 缺少 {p}"));
            }
        }
        for p in po.keys() {
            if !all.contains(&p.as_str()) {
                bad.push(format!("check.{name} 没有参数 {p}"));
            }
        }
        used.extend(slots(params));
    }
    for s in used.difference(keys) {
        bad.push(format!("check 里的 {{{s}}} 不是 fields 里的字段"));
    }
    used
}

// ---------- 给前端和其他模块用 ----------
/// 英文界面时，用清单 en 里写了的英文换掉中文
pub fn en_of(it: &Value) -> Value {
    let en = &it["en"];
    if !py::truthy(en) || i18n::lang() != "en" {
        return it.clone();
    }
    let mut o = it.clone();
    for k in EN_TEXT.iter().chain(EN_LIST.iter()) {
        if let Some(v) = en.get(*k) {
            o[*k] = v.clone();
        }
    }
    if let Some(cp) = en.get("copy") {
        o["copy"] = py::merge(&it["copy"], cp);
    }
    if let Some(ef) = en.get("fields") {
        o["fields"] = json!(it["fields"].as_array().cloned().unwrap_or_default().iter().map(|f| py::merge(f, &ef[f["key"].as_str().unwrap_or("")])).collect::<Vec<_>>());
    }
    if let Some(ec) = en.get("check") {
        let mut m = Map::new();
        for (n, p) in it["check"].as_object().cloned().unwrap_or_default() {
            m.insert(n.clone(), py::merge(&p, &ec[n.as_str()]));
        }
        o["check"] = Value::Object(m);
    }
    o
}

pub fn public() -> Vec<Value> {
    items()
        .values()
        .map(en_of)
        .map(|it| {
            let mut o: Map<String, Value> = it.as_object().cloned().unwrap_or_default().into_iter().filter(|(k, _)| !["connect", "check", "order", "en"].contains(&k.as_str())).collect();
            let fields: Vec<Value> = it["fields"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|f| Value::Object(f.as_object().cloned().unwrap_or_default().into_iter().filter(|(k, _)| k != "pattern" && k != "nospace").collect()))
                .collect();
            o.insert("fields".into(), json!(fields));
            o.entry("example").or_insert(json!(""));
            o.entry("about").or_insert(json!(""));
            o.insert("remote".into(), json!(it["connect"]["type"] != "stdio"));
            Value::Object(o)
        })
        .collect()
}

/// 有应用的分类，按显示顺序：[[键, 名字], …]
pub fn categories() -> Vec<Value> {
    let used: BTreeSet<String> = items().values().filter_map(|i| i["category"].as_str().map(String::from)).collect();
    CATEGORIES.iter().filter(|(k, _)| used.contains(*k)).map(|(k, v)| json!([k, t!(v)])).collect()
}

pub fn title(name: &str) -> String {
    let a = crate::mcp_hub::get(name).unwrap_or(json!({}));
    let key = a["catalog"].as_str().filter(|s| !s.is_empty()).unwrap_or(name).to_string();
    match items().get(&key) {
        Some(i) => py::str_of(&en_of(i)["title"]),
        None => format!("@{name}"),
    }
}

pub fn fill(v: &Value, subst: &HashMap<String, String>) -> Value {
    match v {
        Value::String(s) => json!(SLOT.replace_all(s, |c: &regex::Captures| subst.get(&c[1]).cloned().unwrap_or_else(|| c[0].to_string())).into_owned()),
        Value::Array(a) => json!(a.iter().map(|x| fill(x, subst)).collect::<Vec<_>>()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), fill(x, subst))).collect()),
        other => other.clone(),
    }
}

/// 检查用户填的内容，返回整理好的值
fn values_of(it: &Value, values: &Value) -> Result<HashMap<String, String>, String> {
    let mut vals = HashMap::new();
    for f in it["fields"].as_array().cloned().unwrap_or_default() {
        let key = f["key"].as_str().unwrap_or("").to_string();
        let label = py::str_of(&f["label"]);
        let raw = if py::truthy(&values[key.as_str()]) { &values[key.as_str()] } else if py::truthy(&f["default"]) { &f["default"] } else { &Value::Null };
        let mut v = if raw.is_null() { String::new() } else { py::str_of(raw) }.trim().to_string();
        if py::truthy(&f["nospace"]) {
            v = v.split_whitespace().collect();
        }
        if v.is_empty() && !py::truthy(&f["optional"]) {
            return Err(t!("请填写{label}", label = label));
        }
        if let Some(p) = f["pattern"].as_str().filter(|p| !p.is_empty()) {
            if !v.is_empty() && !fancy_regex::Regex::new(p).ok().and_then(|r| r.is_match(&v).ok()).unwrap_or(false) {
                return Err(match f["invalid"].as_str().filter(|s| !s.is_empty()) {
                    Some(inv) => inv.to_string(),
                    None => t!("{label}的格式不对", label = label),
                });
            }
        }
        if REF.is_match(&v) {
            return Err(t!("{label}里不能写 env: 或 vault: 引用", label = label));
        }
        vals.insert(key, v);
    }
    Ok(vals)
}

/// 添加前先按清单里的 check 试一下填的东西对不对
pub async fn check(item_id: &str, values: &Value) -> Result<(), String> {
    let it = en_of(&items().get(item_id).cloned().unwrap_or(json!({})));
    let Some(ck) = it["check"].as_object().filter(|o| !o.is_empty()).cloned() else { return Ok(()) };
    let vals = values_of(&it, values)?;
    for (name, params) in ck {
        checks::run(&name, &fill(&params, &vals)).await?;
    }
    Ok(())
}

/// 按清单里的模板生成应用配置。用户填的密钥存进保险库
pub fn build(item_id: &str, values: &Value) -> Result<Value, String> {
    let it = items().get(item_id).cloned().ok_or_else(|| t!("应用市场里没有这个应用"))?;
    let vals = values_of(&en_of(&it), values)?;
    let mut subst = HashMap::new();
    let mut owned: Vec<String> = vec![];
    for f in it["fields"].as_array().cloned().unwrap_or_default() {
        let key = f["key"].as_str().unwrap_or("").to_string();
        let mut v = vals.get(&key).cloned().unwrap_or_default();
        if py::truthy(&f["secret"]) && !v.is_empty() {
            let r = format!("{item_id}_{key}").to_uppercase().replace('-', "_");
            vault::put(&r, &v)?;
            owned.push(r.clone());
            v = format!("{{vault:{r}}}");
        }
        subst.insert(key, v);
    }
    let mut cfg = fill(&it["connect"], &subst);
    cfg["catalog"] = json!(item_id);
    cfg["deny"] = it["deny"].clone();
    cfg["confirm"] = it["confirm"].clone();
    cfg["owned"] = json!(owned);
    Ok(cfg)
}

/// 检查所有清单（python -m osa.catalog 的 Rust 版）：有错的清单和原因
pub fn errors() -> Vec<String> {
    load().1
}

pub fn routes() -> axum::Router {
    axum::Router::new()
}
