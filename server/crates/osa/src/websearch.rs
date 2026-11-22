//! 搜索网页（web_search）、读网页（fetch_url）、搜帖子（search_posts）：按“配方”调各家的接口。
//! 对应 osa/websearch.py（配方的写法见那里的说明），默认配方 osa/websearch.yaml 编进二进制。

use crate::generation::{first, http_error, pick, render, render_str};
use crate::{config, i18n, memory, providers, py, vault};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

pub const KINDS: [&str; 3] = ["search", "fetch", "posts"];
pub const COUNT: usize = 8;
pub const POSTS: i64 = 10;
pub const TEXT: usize = 300;
pub const MAX_CHARS: usize = 8000;
pub const BUDGET: f64 = 0.05;
const UA: &str = "open-super-agent/0.1";

static DEFAULTS: LazyLock<Value> = LazyLock::new(|| {
    let y: serde_yaml::Value = serde_yaml::from_str(include_str!("../../../../osa/websearch.yaml")).expect("websearch.yaml");
    config::yaml_to_json(y)
});
static DOMAIN: LazyLock<fancy_regex::Regex> =
    LazyLock::new(|| fancy_regex::Regex::new(r"^(?=.{1,253}$)([a-z0-9-]{1,63}\.)+[a-z0-9-]{2,63}$").unwrap());

/// 这一家这次不行（出错、限流、回包不对），换下一家
#[derive(Debug)]
pub struct Failed(pub String);

type R<T> = Result<T, Failed>;

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

// ---------- 配方和设置 ----------
fn cfg() -> Value {
    let c = config::get("web");
    if c.is_object() { c } else { json!({}) }
}

pub fn recipes(kind: Option<&str>) -> Vec<Value> {
    let mine: Vec<Value> = cfg()["recipes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.is_object() && py::truthy(&r["name"]) && r["kind"].as_str().is_some_and(|k| KINDS.contains(&k)))
        .collect();
    let have: Vec<(Value, Value)> = mine.iter().map(|r| (r["kind"].clone(), r["name"].clone())).collect();
    let mut out = mine;
    for r in DEFAULTS["recipes"].as_array().cloned().unwrap_or_default() {
        if !have.contains(&(r["kind"].clone(), r["name"].clone())) {
            out.push(r);
        }
    }
    out.into_iter().filter(|r| kind.is_none_or(|k| r["kind"] == k)).collect()
}

pub fn recipe(kind: &str, name: &str) -> Option<Value> {
    recipes(Some(kind)).into_iter().find(|r| r["name"] == name)
}

fn saved() -> Map<String, Value> {
    memory::setting("web").and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn store(d: &Map<String, Value>) {
    crate::db::exec("insert or replace into settings values('web', ?)", rusqlite::params![py::dumps_u(&Value::Object(d.clone()))]);
}

fn tr(r: &Value, k: &str) -> String {
    if i18n::lang() == "en"
        && let Some(v) = r["en"][k].as_str().filter(|s| !s.is_empty())
    {
        return v.to_string();
    }
    r[k].as_str().map(String::from).unwrap_or_else(|| if r[k].is_null() { String::new() } else { py::str_of(&r[k]) })
}

fn vault_name(name: &str, field: &str) -> String {
    format!("WEB_{}", re(r"\W").replace_all(&format!("{name}_{field}").to_uppercase(), "_"))
}

pub fn field_value(r: &Value, f: &Value) -> String {
    let name = py::str_of(&r["name"]);
    let key = py::str_of(&f["key"]);
    let mut v: Value = if py::truthy(&f["secret"]) {
        vault::get(&vault_name(&name, &key)).map(Value::String).unwrap_or(Value::Null)
    } else {
        saved().get("fields").and_then(|x| x.get(&name)).and_then(|x| x.get(&key)).cloned().unwrap_or(Value::Null)
    };
    if !py::truthy(&v) && py::truthy(&f["env"]) {
        v = json!(std::env::var(py::str_of(&f["env"])).unwrap_or_default());
    }
    if !py::truthy(&v) && py::truthy(&f["config"]) {
        v = config::get(&py::str_of(&f["config"]));
    }
    if !py::truthy(&v) {
        v = f.get("default").cloned().unwrap_or(Value::Null);
    }
    if py::truthy(&v) { py::str_of(&v).trim().to_string() } else { String::new() }
}

/// None：配方不用模型服务；Some("")：要用但没有配好的
fn provider_of(r: &Value) -> Option<String> {
    let want = r["key"]["provider"].as_str().filter(|s| !s.is_empty())?;
    let rx = Regex::new(&format!("(?i){want}")).ok();
    for (name, p) in config::get("providers").as_object().cloned().unwrap_or_default() {
        if rx.as_ref().is_some_and(|x| x.is_match(p["base_url"].as_str().unwrap_or(""))) && providers::usable(&name) {
            return Some(name);
        }
    }
    Some(String::new())
}

pub fn missing(r: &Value) -> String {
    if provider_of(r).as_deref() == Some("") {
        return t!("要先在 设置 → 模型 里添加 {name} 的模型服务", name = tr(r, "title"));
    }
    for f in r["fields"].as_array().cloned().unwrap_or_default() {
        if !py::truthy(&f["optional"]) && field_value(r, &f).is_empty() {
            let l = tr(&f, "label");
            return t!("还没填：{label}", label = if l.is_empty() { py::str_of(&f["key"]) } else { l });
        }
    }
    String::new()
}

pub fn chosen(kind: &str) -> Vec<String> {
    let s = saved();
    let v = s.get(kind).filter(|v| py::truthy(v)).cloned().unwrap_or_else(|| cfg()[kind].clone());
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn legacy_seen(kind: &str) -> Vec<&'static str> {
    match kind {
        "search" => vec!["deepseek", "tavily", "brave", "bocha", "searxng", "duckduckgo"],
        "fetch" => vec!["deepseek", "jina", "direct"],
        "posts" => vec!["x", "scrapecreators", "reddit-public", "xiaohongshu-browser"],
        _ => vec![],
    }
}

fn seen(kind: &str) -> Option<Vec<String>> {
    let s = saved();
    if !s.contains_key(kind) {
        return None;
    }
    match s.get(&format!("{kind}_seen")).and_then(|v| v.as_array()) {
        Some(a) => Some(a.iter().map(py::str_of).collect()),
        None => Some(legacy_seen(kind).into_iter().map(String::from).collect()),
    }
}

fn auto_of(r: &Value) -> bool {
    r.get("auto").map(py::truthy).unwrap_or(true)
}

fn on(kind: &str, r: &Value, names: &[String]) -> bool {
    if names.is_empty() {
        return auto_of(r);
    }
    let n = py::str_of(&r["name"]);
    if names.contains(&n) {
        return true;
    }
    seen(kind).is_some_and(|s| !s.contains(&n)) && auto_of(r)
}

pub fn order(kind: &str) -> Vec<Value> {
    let names = chosen(kind);
    let mut rs: Vec<Value> = names.iter().filter_map(|n| recipe(kind, n)).collect();
    for r in recipes(Some(kind)) {
        if !names.contains(&py::str_of(&r["name"])) && on(kind, &r, &names) {
            rs.push(r);
        }
    }
    rs.into_iter().filter(|r| missing(r).is_empty()).collect()
}

pub fn block_list() -> Vec<String> {
    let s = saved();
    let v = if s.contains_key("block") { s["block"].clone() } else { cfg()["block"].clone() };
    v.as_array()
        .map(|a| a.iter().map(|x| py::str_of(x).trim().to_lowercase().trim_matches('.').to_string()).filter(|d| !d.is_empty()).collect())
        .unwrap_or_default()
}

pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_lowercase())).unwrap_or_default()
}

/// 网址是不是在不用的网站里（域名本身和它的子域名），是的话返回那一项
pub fn blocked_domain(url: &str) -> Option<String> {
    let host = host_of(url);
    let host = host.trim_end_matches('.');
    block_list().into_iter().find(|d| host == d || host.ends_with(&format!(".{d}")))
}

pub fn refuse_text(url: &str, fetching: bool) -> String {
    let h = host_of(url);
    let host = if h.is_empty() { url.to_string() } else { h };
    if fetching {
        format!("没抓取：{host} 在不用的网站里（设置 → 搜索和读网页），不要从这个网站找信息")
    } else {
        format!("没打开：{host} 在不用的网站里（设置 → 搜索和读网页），不要从这个网站找信息")
    }
}

// ---------- 调接口 ----------
fn vars(r: &Value, kw: &[(&str, Value)]) -> Map<String, Value> {
    let mut v = Map::new();
    for (k, x) in kw {
        if !x.is_null() {
            v.insert(k.to_string(), x.clone());
        }
    }
    let mut key = String::new();
    if let Some(name) = provider_of(r).filter(|n| !n.is_empty()) {
        let b = providers::provider(&name).and_then(|p| p["base_url"].as_str().map(String::from)).unwrap_or_default();
        v.insert("origin".into(), json!(crate::generation::origin_of(&b)));
        key = providers::api_key(&name);
    }
    for f in r["fields"].as_array().cloned().unwrap_or_default() {
        let val = field_value(r, &f);
        if !val.is_empty() {
            let k = py::str_of(&f["key"]);
            v.insert(k.clone(), json!(if k == "url" { val.trim_end_matches('/').to_string() } else { val.clone() }));
            if py::truthy(&f["secret"]) && key.is_empty() {
                key = val;
            }
        }
    }
    if !key.is_empty() {
        v.insert("bearer".into(), json!(format!("Bearer {key}")));
    }
    if let Some(days) = v.get("days").and_then(py::as_int).filter(|d| *d != 0) {
        let today = chrono::Local::now().date_naive();
        v.insert("since".into(), json!((today - chrono::Duration::days(days)).format("%Y-%m-%d").to_string()));
        v.insert("today".into(), json!(today.format("%Y-%m-%d").to_string()));
        let start = chrono::Utc::now() - chrono::Duration::days(days) + chrono::Duration::minutes(1);
        v.insert("since_time".into(), json!(start.format("%Y-%m-%dT%H:%M:%SZ").to_string()));
        let bucket =
            [(1, "day"), (7, "week"), (31, "month"), (366, "year")].into_iter().find(|(n, _)| days <= *n).map(|x| x.1).unwrap_or("year");
        v.insert("range".into(), r["ranges"].get(bucket).cloned().unwrap_or(json!(bucket)));
    }
    v
}

fn client(timeout: f64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout))
        .redirect(reqwest::redirect::Policy::limited(20))
        .build()
        .unwrap_or_default()
}

fn merge_req(base: &Value, over: &Value) -> Value {
    py::merge(base, over)
}

/// 发一次请求，返回 (状态, 正文)
async fn send(c: &reqwest::Client, r: &Value, v: &Map<String, Value>) -> R<String> {
    let mut req = if r["request"].is_object() { r["request"].clone() } else { json!({}) };
    let days = v.get("days").and_then(py::as_int).unwrap_or(0);
    if days != 0 && py::truthy(&r["long"]) && days > py::to_int(&r["long_after"]).unwrap_or(0) {
        req = merge_req(&req, &r["long"]);
    } else if days != 0 && py::truthy(&r["recent"]) {
        req = merge_req(&req, &r["recent"]);
    }
    let url = py::str_of(&{
        let u = render(&req.get("url").cloned().unwrap_or(json!("")), v);
        if u.is_null() { json!("") } else { u }
    });
    let method = req["method"].as_str().filter(|s| !s.is_empty()).unwrap_or("POST").to_uppercase();
    let mut rb = c.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::POST), &url).header("User-Agent", UA);
    if req.get("params").is_some() {
        let ps: Vec<(String, String)> =
            render(&req["params"], v).as_object().cloned().unwrap_or_default().into_iter().map(|(k, x)| (k, py::str_of(&x))).collect();
        rb = rb.query(&ps);
    }
    if req.get("json").is_some() {
        rb = rb.json(&render(&req["json"], v));
    } else if req.get("form").is_some() {
        let fs: Vec<(String, String)> =
            render(&req["form"], v).as_object().cloned().unwrap_or_default().into_iter().map(|(k, x)| (k, py::str_of(&x))).collect();
        rb = rb.form(&fs);
    }
    for (k, x) in render(&req.get("headers").cloned().unwrap_or(json!({})), v).as_object().cloned().unwrap_or_default() {
        rb = rb.header(k, py::str_of(&x));
    }
    let resp = rb.send().await.map_err(|e| {
        if e.is_timeout() {
            Failed(t!("连接超时"))
        } else {
            Failed(t!("连不上：{error}", error = if e.is_connect() { "ConnectError" } else { "HTTPError" }))
        }
    })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status == 429 {
        return Err(Failed(t!("被限流了（HTTP 429）")));
    }
    if status >= 400 {
        return Err(Failed(http_error(status, &text, "")));
    }
    Ok(text)
}

fn json_of(text: &str) -> R<Value> {
    serde_json::from_str(text).map_err(|_| Failed(t!("返回的不是 JSON：{text}", text = py::head(text, 120))))
}

pub fn unescape(s: &str) -> String {
    static ENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&(#[xX]?[0-9a-fA-F]+|[a-zA-Z]+);?").unwrap());
    ENT.replace_all(s, |c: &regex::Captures| {
        let e = &c[1];
        if let Some(n) = e.strip_prefix('#') {
            let code = if let Some(h) = n.strip_prefix(['x', 'X']) { u32::from_str_radix(h, 16).ok() } else { n.parse().ok() };
            return code.and_then(char::from_u32).map(String::from).unwrap_or_else(|| c[0].to_string());
        }
        match e {
            "amp" => "&".into(),
            "lt" => "<".into(),
            "gt" => ">".into(),
            "quot" => "\"".into(),
            "apos" => "'".into(),
            "nbsp" => "\u{a0}".into(),
            "mdash" => "—".into(),
            "ndash" => "–".into(),
            "hellip" => "…".into(),
            "rsquo" => "’".into(),
            "lsquo" => "‘".into(),
            "rdquo" => "”".into(),
            "ldquo" => "“".into(),
            "middot" => "·".into(),
            "copy" => "©".into(),
            _ => c[0].to_string(),
        }
    })
    .into_owned()
}

pub fn clean(s: &Value) -> String {
    let s = if py::truthy(s) { py::str_of(s) } else { String::new() };
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    WS.replace_all(&unescape(&TAG.replace_all(&s, "")), " ").trim().to_string()
}

fn ddg(text: &str) -> R<Vec<Value>> {
    static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)class="result__a" href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap());
    static SNIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).unwrap());
    let links: Vec<(String, String)> = LINK.captures_iter(text).map(|c| (c[1].to_string(), c[2].to_string())).collect();
    let snippets: Vec<String> = SNIP.captures_iter(text).map(|c| c[1].to_string()).collect();
    if links.is_empty() && text.contains("anomaly") {
        return Err(Failed(t!("DuckDuckGo 要人机验证（请求太多被拦了）")));
    }
    let mut out = vec![];
    for (i, (u, tl)) in links.iter().enumerate() {
        let mut u = unescape(u);
        if u.starts_with("//") {
            u = format!("https:{u}");
        }
        if u.contains("uddg=")
            && let Ok(p) = reqwest::Url::parse(&u)
            && let Some((_, x)) = p.query_pairs().find(|(k, _)| k == "uddg")
        {
            u = x.into_owned();
        }
        if host_of(&u).ends_with("duckduckgo.com") {
            continue;
        }
        out.push(json!({"title": clean(&json!(tl)), "url": u, "text": snippets.get(i).map(|s| clean(&json!(s))).unwrap_or_default(), "date": null}));
    }
    Ok(out)
}

const POST_FIELDS: [&str; 5] = ["author", "likes", "comments", "shares", "views"];

fn get_spec(x: &Value, spec: &Value) -> Value {
    static V: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([\w.]+)\}").unwrap());
    let list: Vec<Value> = match spec {
        Value::Array(a) => a.clone(),
        s => vec![s.clone()],
    };
    for s in list {
        let Some(s) = s.as_str() else { continue };
        if s.contains("${") {
            let parts: HashMap<String, Value> = V.captures_iter(s).map(|c| (c[1].to_string(), pick(x, &c[1]))).collect();
            if parts.values().all(|v| !v.is_null() && v.as_str() != Some("")) {
                return json!(V.replace_all(s, |c: &regex::Captures| py::str_of(&parts[&c[1]])).into_owned());
            }
        } else {
            let v = pick(x, s);
            if !v.is_null() && v.as_str() != Some("") {
                return v;
            }
        }
    }
    Value::Null
}

fn or_spec(item: &Value, k: &str) -> Value {
    if py::truthy(&item[k]) { item[k].clone() } else { json!(k) }
}

fn items(r: &Value, d: &Value) -> R<Vec<Value>> {
    let paths = if py::truthy(&r["results"]) { r["results"].clone() } else { json!("results") };
    let list: Vec<Value> = match &paths {
        Value::Array(a) => a.clone(),
        p => vec![p.clone()],
    };
    let found = list.iter().filter_map(|p| p.as_str()).map(|p| pick(d, p)).find(|x| x.is_array());
    let Some(found) = found else {
        if d.is_object() {
            let e = if py::truthy(&d["errors"]) { d["errors"].clone() } else { d["error"].clone() };
            if py::truthy(&e) {
                let e = match &e {
                    Value::Array(a) if !a.is_empty() => a[0].clone(),
                    e => e.clone(),
                };
                let m = if e.is_object() {
                    let x = if py::truthy(&e["message"]) {
                        e["message"].clone()
                    } else if py::truthy(&e["detail"]) {
                        e["detail"].clone()
                    } else {
                        e.clone()
                    };
                    py::str_of(&x)
                } else {
                    py::str_of(&e)
                };
                return Err(Failed(py::head(&m, 200)));
            }
        }
        return Ok(vec![]);
    };
    let item = if r["item"].is_object() { r["item"].clone() } else { json!({}) };
    let mut out = vec![];
    for x in found.as_array().cloned().unwrap_or_default() {
        if !x.is_object() {
            continue;
        }
        let u = get_spec(&x, &or_spec(&item, "url"));
        let Some(u) = u.as_str().filter(|u| u.starts_with("http://") || u.starts_with("https://")) else { continue };
        let mut e = json!({"title": clean(&get_spec(&x, &or_spec(&item, "title"))), "url": u, "text": clean(&get_spec(&x, &or_spec(&item, "text"))),
                           "date": get_spec(&x, &or_spec(&item, "date"))});
        for k in POST_FIELDS {
            if item.get(k).is_some() {
                e[k] = get_spec(&x, &item[k]);
            }
        }
        out.push(e);
    }
    Ok(out)
}

/// 调一次 MCP 服务的工具（Exa 的免费搜索），返回它给的文字。每次新开一个会话
async fn mcp_call(url: &str, tool: &str, args: &Value, timeout: f64) -> R<String> {
    use crate::mcp_client::{Session, Transport};
    let t = Duration::from_secs_f64(timeout);
    let s = Session::connect(Transport::Http { url: url.into(), headers: HashMap::new(), auth: None }, json!({}), t)
        .await
        .map_err(|e| mcp_err(&e.to_string()))?;
    let res =
        tokio::time::timeout(t, s.call_tool(tool, args)).await.map_err(|_| Failed(t!("连接超时")))?.map_err(|e| mcp_err(&e.to_string()));
    s.shutdown().await;
    let res = res?;
    let text: Vec<String> = res["content"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x["text"].as_str().filter(|t| !t.is_empty()).map(String::from)).collect())
        .unwrap_or_default();
    let text = text.join("\n");
    if py::truthy(&res["isError"]) {
        return Err(Failed(if text.is_empty() { t!("服务出错了") } else { py::head(&text, 200) }));
    }
    Ok(text)
}

fn mcp_err(msg: &str) -> Failed {
    if msg == "timeout" {
        return Failed(t!("连接超时"));
    }
    Failed(if msg.contains("429") { t!("被限流了（HTTP 429）") } else { py::head(msg, 160) })
}

async fn send_mcp(r: &Value, v: &Map<String, Value>) -> R<String> {
    let mut req = if r["request"].is_object() { r["request"].clone() } else { json!({}) };
    if v.get("days").is_some_and(py::truthy) && py::truthy(&r["recent"]) {
        req = merge_req(&req, &r["recent"]);
    }
    mcp_call(
        req["mcp"].as_str().unwrap_or(""),
        req["tool"].as_str().unwrap_or(""),
        &render(&req.get("args").cloned().unwrap_or(json!({})), v),
        30.0,
    )
    .await
}

fn exa_blocks(text: &str) -> Vec<Value> {
    static SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n-{3,}\n").unwrap());
    let mut out = vec![];
    for block in SPLIT.split(text) {
        let get = |k: &str| -> String {
            Regex::new(&format!(r"(?m)^{k}:[ \t]*(.*)$"))
                .ok()
                .and_then(|r| r.captures(block).map(|c| c[1].trim().to_string()))
                .unwrap_or_default()
        };
        let url = get("URL");
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let hl = block.split_once("Highlights:").map(|x| x.1).unwrap_or("");
        let date = get("Published");
        let author = get("Author");
        out.push(json!({"title": clean(&json!(get("Title"))), "url": url, "text": clean(&json!(hl)),
                        "date": if date.is_empty() || date == "N/A" { Value::Null } else { json!(date) },
                        "author": if author.is_empty() || author == "N/A" { String::new() } else { author }}));
    }
    out
}

static X_ID: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?:(?:^|//|\.)(?:x|twitter|xcancel|fxtwitter|vxtwitter|fixupx|fixvx)\.com/[^/\s]+/status(?:es)?/|//(?:[\w-]+\.)?(?:nitter|xitter)\.[\w.]+/[^/\s]+/status/|unrollnow\.com/status/|threadreaderapp\.com/thread/)(\d{8,})",
    )
});
static REDDIT_THREAD: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^https?://(?:www\.|old\.)?reddit\.com/r/([^/]+)/comments/\w+"));

fn since_ok(date: &Value, days: Option<i64>) -> bool {
    static D: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{4}-\d{2}-\d{2}$"));
    let d = date_of(date);
    let Some(days) = days.filter(|d| *d != 0) else { return true };
    if !D.is_match(&d) {
        return true;
    }
    d >= (chrono::Local::now().date_naive() - chrono::Duration::days(days)).format("%Y-%m-%d").to_string()
}

fn tco_tail(text: &str) -> String {
    static T: LazyLock<Regex> = LazyLock::new(|| re(r"\s*https://t\.co/\w+$"));
    T.replace(text, "").into_owned()
}

fn x_body(t: &Value) -> String {
    let nt = &t["note_tweet"]["text"];
    let mut text = if py::truthy(nt) {
        py::str_of(nt)
    } else if py::truthy(&t["text"]) {
        py::str_of(&t["text"])
    } else {
        String::new()
    };
    for u in t["entities"]["urls"].as_array().cloned().unwrap_or_default() {
        if let (Some(a), Some(b)) = (u["url"].as_str().filter(|s| !s.is_empty()), u["expanded_url"].as_str().filter(|s| !s.is_empty())) {
            text = text.replace(a, b);
        }
    }
    tco_tail(&text)
}

async fn x_details(c: &reqwest::Client, tid: &str) -> Option<Value> {
    let r = c
        .get(format!("https://cdn.syndication.twimg.com/tweet-result?id={tid}&token={}", x_token(tid)))
        .header("User-Agent", UA)
        .send()
        .await
        .ok()?;
    if r.status().as_u16() != 200 {
        return None;
    }
    let d: Value = r.json().await.ok()?;
    if !d.is_object() || !py::truthy(&d["text"]) {
        return None;
    }
    let user = &d["user"];
    let sn = user["screen_name"].as_str().filter(|s| !s.is_empty());
    Some(json!({"title": "", "url": format!("https://x.com/{}/status/{tid}", sn.unwrap_or("i")), "text": clean(&json!(x_body(&d))),
                "date": d["created_at"], "author": sn.map(|s| format!("@{s}")).unwrap_or_default(),
                "likes": d["favorite_count"], "comments": d["conversation_count"]}))
}

fn recent(items: Vec<Value>, days: Option<i64>) -> R<Vec<Value>> {
    let n = items.len();
    let keep: Vec<Value> = items.into_iter().filter(|x| since_ok(&x["date"], days)).collect();
    if n > 0 && keep.is_empty() {
        return Err(Failed(t!("找到 {n} 条，但都早于最近 {days} 天", n = n, days = days.map(|d| json!(d)).unwrap_or(Value::Null))));
    }
    Ok(keep)
}

async fn exa_x(c: &reqwest::Client, text: &str, days: Option<i64>, count: usize) -> R<Vec<Value>> {
    let mut ids: Vec<String> = vec![];
    for b in exa_blocks(text) {
        if let Some(m) = X_ID.captures(b["url"].as_str().unwrap_or(""))
            && !ids.contains(&m[1].to_string())
        {
            ids.push(m[1].to_string());
        }
    }
    ids.truncate(count.max(10));
    let found = futures::future::join_all(ids.iter().map(|i| x_details(c, i))).await;
    let mut r = recent(found.into_iter().flatten().collect(), days)?;
    r.truncate(count);
    Ok(r)
}

fn exa_reddit(text: &str, days: Option<i64>) -> R<Vec<Value>> {
    let threads: Vec<Value> = exa_blocks(text)
        .into_iter()
        .filter_map(|b| {
            let u = b["url"].as_str().unwrap_or("").to_string();
            let m = REDDIT_THREAD.captures(&u)?;
            let mut b = b.clone();
            b["author"] = json!(format!("r/{}", &m[1]));
            b["url"] = json!(u.split('?').next().unwrap_or(&u));
            Some(b)
        })
        .collect();
    recent(threads, days)
}

async fn search_one(c: &reqwest::Client, r: &Value, query: &str, days: Option<i64>, count: usize) -> R<Vec<Value>> {
    let v = vars(r, &[("query", json!(query)), ("count", json!(count)), ("days", days.map(|d| json!(d)).unwrap_or(Value::Null))]);
    if py::truthy(&r["request"]["mcp"]) {
        let text = send_mcp(r, &v).await?;
        return match r["parse"].as_str() {
            Some("exa_x") => exa_x(c, &text, days, count).await,
            Some("exa_reddit") => Ok(exa_reddit(&text, days)?.into_iter().take(count).collect()),
            _ => Ok(exa_blocks(&text).into_iter().filter(|b| since_ok(&b["date"], days)).take(count).collect()),
        };
    }
    let text = send(c, r, &v).await?;
    if r["parse"] == "duckduckgo" {
        return ddg(&text);
    }
    items(r, &json_of(&text)?)
}

fn comment_lines(items: &Value, depth: usize, out: &mut Vec<String>, budget: &mut i64) {
    for x in items.as_array().cloned().unwrap_or_default() {
        if *budget <= 0 || !x.is_object() {
            return;
        }
        let body = clean(&x["body"]);
        if !body.is_empty() && body != "[deleted]" && body != "[removed]" {
            *budget -= 1;
            let score = if x.get("score").is_some() { x["score"].clone() } else { x["ups"].clone() };
            let author = if py::truthy(&x["author"]) { py::str_of(&x["author"]) } else { "?".into() };
            let sc = if score.is_null() || score.as_str() == Some("") { String::new() } else { format!("（{}）", num(&score)) };
            out.push(format!("{}- u/{author}{sc}：{}", "  ".repeat(depth), py::head(&body, TEXT)));
        }
        if depth < 1 {
            comment_lines(&x["replies"]["items"], depth + 1, out, budget);
        }
    }
}

fn reddit_comments(d: &Value) -> R<(String, String)> {
    let post = if d.is_object() && d["post"].is_object() { d["post"].clone() } else { json!({}) };
    if post.as_object().is_none_or(|o| o.is_empty()) && !py::truthy(&d["comments"]) {
        return Err(Failed(t!("回包里没有帖子")));
    }
    let f = |k: &str, fmt: &dyn Fn(String) -> String| if py::truthy(&post[k]) { fmt(py::str_of(&post[k])) } else { String::new() };
    let meta = [
        f("subreddit", &|s| format!("r/{s}")),
        f("author", &|s| format!("u/{s}")),
        date_of(&post["created_utc"]),
        if py::truthy(&post["score"]) { format!("{} {}", t!("赞"), num(&post["score"])) } else { String::new() },
        if py::truthy(&post["num_comments"]) { format!("{} {}", t!("评论"), num(&post["num_comments"])) } else { String::new() },
    ];
    let mut lines = vec![meta.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ")];
    if py::truthy(&post["selftext"]) {
        lines.push(String::new());
        lines.push(py::str_of(&post["selftext"]).trim().to_string());
    }
    let mut comments = vec![];
    let mut budget = 40;
    comment_lines(&d["comments"], 0, &mut comments, &mut budget);
    if !comments.is_empty() {
        lines.push(String::new());
        lines.push(format!("## {}", t!("评论")));
        lines.extend(comments);
    }
    let title = if py::truthy(&post["title"]) { py::str_of(&post["title"]).trim().to_string() } else { String::new() };
    Ok((title, lines.join("\n")))
}

/// X 嵌入帖子的地址要带的 token
pub fn x_token(tid: &str) -> String {
    let x = tid.parse::<f64>().unwrap_or(0.0) / 1e15 * std::f64::consts::PI;
    let mut whole = x.trunc() as u64;
    let mut frac = x - x.trunc();
    let digits: Vec<char> = "0123456789abcdefghijklmnopqrstuvwxyz".chars().collect();
    let mut out = String::new();
    while whole > 0 {
        out.insert(0, digits[(whole % 36) as usize]);
        whole /= 36;
    }
    if out.is_empty() {
        out.push('0');
    }
    out.push('.');
    for _ in 0..11 {
        frac *= 36.0;
        out.push(digits[frac.trunc() as usize % 36]);
        frac -= frac.trunc();
    }
    let r = re(r"0+|\.").replace_all(&out, "").into_owned();
    if r.is_empty() { "a".into() } else { r }
}

fn x_post(d: &Value) -> R<(String, String)> {
    if !d.is_object() || !(py::truthy(&d["text"]) || py::truthy(&d["note_tweet"])) {
        return Err(Failed(t!("没有这条帖子（可能删了，或者要登录才能看）")));
    }
    let user = &d["user"];
    let mut meta = vec![date_of(&d["created_at"])];
    for (k, label) in [("favorite_count", t!("赞")), ("conversation_count", t!("回复"))] {
        let n = num(&d[k]);
        if !n.is_empty() && n != "0" {
            meta.push(format!("{label} {n}"));
        }
    }
    let mut lines =
        vec![meta.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · "), String::new(), x_body(d).trim().to_string()];
    if py::truthy(&d["quoted_tweet"]) {
        let q = &d["quoted_tweet"];
        let sn = q["user"]["screen_name"].as_str().unwrap_or("?");
        lines.push(String::new());
        lines.push(format!("> {} @{sn}：{}", t!("引用"), x_body(q).trim()));
    }
    Ok((format!("@{}（{}）", user["screen_name"].as_str().unwrap_or("?"), user["name"].as_str().unwrap_or("")), lines.join("\n")))
}

fn getdaytrends(page: &str) -> R<(String, String)> {
    static TABLE: LazyLock<Regex> = LazyLock::new(|| re(r"<table[\s\S]*?</table>"));
    static ROW: LazyLock<Regex> = LazyLock::new(|| re(r#"class="pos">(\d+)</th>\s*<td class="main"><a[^>]*>([^<]+)</a>"#));
    static H1: LazyLock<Regex> = LazyLock::new(|| re(r"<h1[^>]*>([\s\S]*?)</h1>"));
    static TITLE: LazyLock<Regex> = LazyLock::new(|| re(r"<title>([^<]+)"));
    let rows: Vec<(String, String)> =
        TABLE.find(page).map(|m| ROW.captures_iter(m.as_str()).map(|c| (c[1].to_string(), c[2].to_string())).collect()).unwrap_or_default();
    if rows.is_empty() {
        return Err(Failed(t!("页面里没找到热门话题表")));
    }
    let title = H1.captures(page).or_else(|| TITLE.captures(page)).map(|c| c[1].to_string()).unwrap_or_default();
    let lines: Vec<String> = rows
        .iter()
        .take(30)
        .map(|(n, tp)| {
            let tp = unescape(tp);
            let q: String = url::form_urlencoded::byte_serialize(tp.as_bytes()).collect::<String>().replace('+', "%20");
            format!("{n}. {tp} — https://x.com/search?q={q}")
        })
        .collect();
    Ok((
        clean(&json!(title)),
        format!("{}\n\n{}", t!("（只有话题名和排名，看不到在说什么：要知道的话用 web_search 查话题名）"), lines.join("\n")),
    ))
}

async fn fetch_one(c: &reqwest::Client, r: &Value, url: &str) -> R<(String, String)> {
    if r["parse"] == "direct" {
        return Ok((String::new(), crate::tools::direct_fetch(url).await));
    }
    let mut kw: Vec<(String, Value)> = vec![];
    for (k, rx) in r["url_vars"].as_object().cloned().unwrap_or_default() {
        let m = Regex::new(rx.as_str().unwrap_or("")).ok().and_then(|x| x.captures(url).map(|c| c[1].to_string()));
        match m {
            Some(x) => kw.push((k, json!(x))),
            None => return Err(Failed(t!("这个网址读不了：{url}", url = py::head(url, 80)))),
        }
    }
    if r["parse"] == "x_post" {
        let tid = kw.iter().find(|(k, _)| k == "tweet_id").map(|x| py::str_of(&x.1)).unwrap_or_default();
        kw.push(("x_token".into(), json!(x_token(&tid))));
    }
    let mut all: Vec<(&str, Value)> = vec![("url", json!(url))];
    all.extend(kw.iter().map(|(k, v)| (k.as_str(), v.clone())));
    let text = send(c, r, &vars(r, &all)).await?;
    match r["parse"].as_str() {
        Some("getdaytrends") => getdaytrends(&text),
        Some("x_post") => x_post(&json_of(&text)?),
        Some("reddit_comments") => reddit_comments(&json_of(&text)?),
        _ => {
            let d = json_of(&text)?;
            let page = if r["page"].is_object() { r["page"].clone() } else { json!({}) };
            let title = first(&d, &or_spec(&page, "title"));
            let body = first(&d, &or_spec(&page, "text"));
            Ok((
                if py::truthy(&title) { py::str_of(&title).trim().to_string() } else { String::new() },
                if py::truthy(&body) { py::str_of(&body) } else { String::new() },
            ))
        }
    }
}

pub fn date_of(v: &Value) -> String {
    use chrono::TimeZone;
    if let Some(n) = v.as_f64().filter(|n| *n > 1e8 && !v.is_string()) {
        let secs = if n > 1e11 { n / 1000.0 } else { n };
        return chrono::Utc.timestamp_opt(secs as i64, 0).single().map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
    }
    let s = if py::truthy(v) { py::str_of(v).trim().to_string() } else { String::new() };
    static D: LazyLock<Regex> = LazyLock::new(|| re(r"^\d{4}-\d{2}-\d{2}"));
    if let Some(m) = D.find(&s) {
        return m.as_str().to_string();
    }
    if let Ok(t) = chrono::DateTime::parse_from_str(&s, "%a %b %d %H:%M:%S %z %Y") {
        return t.format("%Y-%m-%d").to_string();
    }
    py::head(&s, 24)
}

fn format_items(items: &[Value]) -> String {
    items
        .iter()
        .take(COUNT)
        .map(|e| {
            let title = e["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(e["url"].as_str().unwrap_or(""));
            let d = date_of(&e["date"]);
            let head =
                format!("- {title} | {}{}", e["url"].as_str().unwrap_or(""), if d.is_empty() { String::new() } else { format!(" | {d}") });
            let text = e["text"].as_str().unwrap_or("");
            head + &if text.is_empty() { String::new() } else { format!("\n  {}", py::head(text, TEXT)) }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn num(v: &Value) -> String {
    let n = match v {
        Value::Number(x) => x.as_i64().or_else(|| x.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    };
    let Some(n) = n else { return String::new() };
    if n >= 10000 { format!("{:.1}万", n as f64 / 10000.0).replace(".0万", "万") } else { n.to_string() }
}

fn format_posts(items: &[Value]) -> String {
    let names = [("likes", t!("赞")), ("comments", t!("评论")), ("shares", t!("转发")), ("views", t!("浏览"))];
    items
        .iter()
        .map(|e| {
            let mut meta = vec![if py::truthy(&e["author"]) { py::str_of(&e["author"]) } else { String::new() }, date_of(&e["date"])];
            for (k, label) in &names {
                let n = num(&e[*k]);
                if !n.is_empty() && n != "0" {
                    meta.push(format!("{label} {n}"));
                }
            }
            let head = format!(
                "- {} | {}",
                meta.into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · "),
                e["url"].as_str().unwrap_or("")
            );
            let body: Vec<String> = ["title", "text"].iter().filter(|k| py::truthy(&e[**k])).map(|k| py::str_of(&e[*k])).collect();
            let body = body.join(" ");
            head + &if body.is_empty() { String::new() } else { format!("\n  {}", py::head(&body, TEXT)) }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn record(r: &Value, what: &str, ms: f64, n: usize) {
    let usd = r["price_usd"].as_f64().unwrap_or(0.0) + r["price_usd_item"].as_f64().unwrap_or(0.0) * n as f64;
    if usd == 0.0 {
        return;
    }
    let prov = provider_of(r).filter(|p| !p.is_empty()).unwrap_or_else(|| py::str_of(&r["name"]));
    memory::add_usage(
        "web",
        what,
        memory::UsageRec { provider: Some(prov), cost: usd * crate::generation::usd_rate(), ms: Some(ms), ..Default::default() },
    );
}

fn tail(notes: &[String], used: &str) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let s = notes.join("；");
    format!(
        "\n{}",
        if used.is_empty() { t!("（{notes}）", notes = s) } else { t!("（{notes}；这次用的是 {name}）", notes = s, name = used) }
    )
}

// ---------- 工具 ----------
pub async fn search(query: &str, days: Option<i64>, only: Option<&Value>) -> String {
    let query = query.trim();
    if query.is_empty() {
        return t!("没有给搜索词");
    }
    let rs = match only {
        Some(r) => vec![r.clone()],
        None => order("search"),
    };
    if rs.is_empty() {
        return t!("搜索不了：没有能用的搜索服务（设置 → 搜索和读网页）");
    }
    let (mut notes, mut empty) = (vec![], 0);
    let c = client(20.0);
    for r in &rs {
        let t0 = Instant::now();
        let items = match search_one(&c, r, query, days, COUNT).await {
            Ok(i) => i,
            Err(Failed(e)) => {
                notes.push(format!("{}：{e}", tr(r, "title")));
                continue;
            }
        };
        if !items.is_empty() {
            record(r, "search", t0.elapsed().as_secs_f64() * 1000.0, 1);
        }
        let items: Vec<Value> = items.into_iter().filter(|e| blocked_domain(e["url"].as_str().unwrap_or("")).is_none()).collect();
        if !items.is_empty() {
            return format_items(&items) + &tail(&notes, &tr(r, "title"));
        }
        empty += 1;
        notes.push(t!("{name}：没有结果", name = tr(r, "title")));
    }
    (if empty > 0 { t!("无结果") } else { t!("搜索失败") }) + &tail(&notes, "")
}

pub async fn fetch(url: &str, private: bool, only: Option<&Value>, browser: bool) -> String {
    let direct = || recipes(Some("fetch")).into_iter().filter(|r| r["parse"] == "direct").take(1).collect::<Vec<_>>();
    let rs: Vec<Value> = if let Some(r) = only {
        vec![r.clone()]
    } else if private {
        direct()
    } else {
        let host = host_of(url);
        let mine = |r: &Value| {
            r["hosts"].as_array().is_some_and(|hs| {
                hs.iter().any(|h| {
                    let h = py::str_of(h);
                    host == h || host.ends_with(&format!(".{h}"))
                })
            })
        };
        let all = order("fetch");
        let mut rs: Vec<Value> = all.iter().filter(|r| mine(r)).cloned().collect();
        rs.extend(all.iter().filter(|r| !py::truthy(&r["hosts"])).cloned());
        if rs.is_empty() { direct() } else { rs }
    };
    let mut notes = vec![];
    let c = client(30.0);
    for r in &rs {
        if r["via"] == "browser" {
            if browser {
                return t!(
                    "{name}：这个网站不登录读不了，用电脑上的浏览器读：browser_open 打开 {url}，用 browser_read 读正文和评论。页面要登录或出现验证码时调用 ask_human，请用户在右栏的电脑画面里登录（只要登一次）。",
                    name = tr(r, "title"),
                    url = url
                );
            }
            notes.push(t!("{name}：要用电脑上的浏览器读，这里没有电脑可用", name = tr(r, "title")));
            continue;
        }
        let t0 = Instant::now();
        let (title, text) = match fetch_one(&c, r, url).await {
            Ok(x) => x,
            Err(Failed(e)) => {
                notes.push(format!("{}：{e}", tr(r, "title")));
                continue;
            }
        };
        if r["parse"] == "direct" && text.starts_with("没抓取") {
            return text;
        }
        static NL: LazyLock<Regex> = LazyLock::new(|| re(r"\n{3,}"));
        let text = NL.replace_all(&text, "\n\n").trim().to_string();
        if !text.is_empty() {
            record(r, "fetch", t0.elapsed().as_secs_f64() * 1000.0, 1);
            let first_line = text.split('\n').next().unwrap_or("");
            let head = if !title.is_empty() && !first_line.contains(&title) { format!("# {title}\n\n") } else { String::new() };
            return head + &py::head(&text, MAX_CHARS) + &tail(&notes, &tr(r, "title"));
        }
        notes.push(t!("{name}：没读到内容", name = tr(r, "title")));
    }
    t!("没读到：{why}", why = if notes.is_empty() { url.to_string() } else { notes.join("；") })
}

pub fn platform_name(p: &str) -> String {
    match p {
        "x" => "X".into(),
        "reddit" => "Reddit".into(),
        "xiaohongshu" => "小红书".into(),
        o => o.to_string(),
    }
}

static SPENT: LazyLock<Mutex<HashMap<String, f64>>> = LazyLock::new(Default::default);

pub fn budget() -> f64 {
    let s = saved();
    let v = if s.contains_key("posts_budget_usd") {
        s["posts_budget_usd"].clone()
    } else {
        cfg().get("posts_budget_usd").cloned().unwrap_or(json!(BUDGET))
    };
    match &v {
        Value::Number(n) => n.as_f64().unwrap_or(BUDGET).max(0.0),
        Value::String(s) => s.trim().parse::<f64>().map(|x| x.max(0.0)).unwrap_or(BUDGET),
        Value::Bool(b) => *b as i64 as f64,
        _ => BUDGET,
    }
}

fn task_key() -> String {
    let w = memory::who();
    if let Some(t) = w.task_id {
        return t.to_string();
    }
    match w.chat_id {
        Some(c) => format!("chat:{c}"),
        None => "other".into(),
    }
}

pub fn platforms() -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for r in recipes(Some("posts")) {
        if let Some(p) = r["platform"].as_str().filter(|s| !s.is_empty())
            && !out.iter().any(|x| x == p)
        {
            out.push(p.to_string());
        }
    }
    out
}

fn platform(p: &str) -> String {
    let p = p.trim().to_lowercase();
    match p.as_str() {
        "twitter" | "推特" => "x".into(),
        "小红书" | "xhs" | "红书" => "xiaohongshu".into(),
        _ => p,
    }
}

pub async fn search_posts(
    plat_in: &str,
    query: &str,
    days: Option<i64>,
    count: Option<i64>,
    only: Option<&Value>,
    browser: bool,
) -> String {
    let query = query.trim();
    if query.is_empty() {
        return t!("没有给搜索词");
    }
    let plat = platform(plat_in);
    let allr: Vec<Value> = recipes(Some("posts")).into_iter().filter(|r| r["platform"] == plat.as_str()).collect();
    if allr.is_empty() {
        return t!("不认识这个平台：{name}。能搜的：{list}", name = plat_in, list = platforms().join("、"));
    }
    let rs: Vec<Value> = match only {
        Some(r) => vec![r.clone()],
        None => order("posts").into_iter().filter(|r| r["platform"] == plat.as_str()).collect(),
    };
    if rs.is_empty() {
        let why: Vec<String> = allr
            .iter()
            .map(|r| {
                let m = missing(r);
                format!("{}：{}", tr(r, "title"), if m.is_empty() { t!("没打开") } else { m })
            })
            .collect();
        return t!("搜不了 {name} 的帖子：{why}（设置 → 搜索和读网页）", name = platform_name(&plat), why = why.join("；"));
    }
    let count = count.filter(|c| *c != 0).unwrap_or(POSTS).clamp(1, 50) as usize;
    let (mut notes, mut empty) = (vec![], 0);
    let c = client(20.0);
    for r in &rs {
        let max_days = py::to_int(&r["max_days"]).filter(|_| py::truthy(&r["max_days"]));
        if let (Some(d), Some(m)) = (days.filter(|d| *d != 0), max_days)
            && d > m
        {
            notes.push(t!("{name}：只能搜最近 {n} 天", name = tr(r, "title"), n = r["max_days"].clone()));
            continue;
        }
        if r["via"] == "browser" {
            if !browser {
                notes.push(t!("{name}：要用电脑上的浏览器搜，这里没有电脑可用", name = tr(r, "title")));
                continue;
            }
            let mut v = Map::new();
            v.insert("query".into(), json!(url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>().replace('+', "%20")));
            let u = render_str(r["url"].as_str().unwrap_or(""), &v);
            return t!(
                "{name} 没有接口，用电脑上的浏览器搜：browser_open 打开 {url}，用 browser_read 读结果，点开笔记看正文和评论。页面要登录或出现验证码时调用 ask_human，请用户在右栏的电脑画面里登录（只要登一次）。别连续快速翻很多页：请求太多会被风控。",
                name = tr(r, "title"),
                url = u
            );
        }
        let min_count = py::to_int(&r["min_count"]).filter(|_| py::truthy(&r["min_count"])).unwrap_or(1).max(0) as usize;
        let mut want = count.max(min_count.max(1));
        let per = r["price_usd_item"].as_f64().unwrap_or(0.0);
        let key = task_key();
        if per > 0.0 {
            let left = budget() - SPENT.lock().get(&key).copied().unwrap_or(0.0);
            let afford = ((left / per) + 1e-9).floor().max(0.0) as usize;
            if afford < min_count.max(1) {
                notes.push(t!("{name}：这个任务搜帖子的预算用完了（每个任务最多 {usd} 美元，设置 → 搜索和读网页 里能改）。用已经搜到的；只想知道在热议什么，用 fetch_url 读 getdaytrends.com 的热门话题榜，再用 web_search 查", name = tr(r, "title"), usd = fmt_g(budget())));
                continue;
            }
            want = want.min(afford);
        }
        let t0 = Instant::now();
        let items = match search_one(&c, r, query, days, want).await {
            Ok(i) => i,
            Err(Failed(e)) => {
                notes.push(format!("{}：{e}", tr(r, "title")));
                continue;
            }
        };
        if !items.is_empty() {
            record(r, "posts", t0.elapsed().as_secs_f64() * 1000.0, items.len());
            if per > 0.0 {
                let mut s = SPENT.lock();
                if s.len() > 500 {
                    s.clear();
                }
                *s.entry(key).or_insert(0.0) += per * items.len() as f64;
            }
        }
        let items: Vec<Value> =
            items.into_iter().filter(|e| blocked_domain(e["url"].as_str().unwrap_or("")).is_none()).take(count).collect();
        if !items.is_empty() {
            return format_posts(&items) + &tail(&notes, &tr(r, "title"));
        }
        empty += 1;
        notes.push(t!("{name}：没有结果", name = tr(r, "title")));
    }
    (if empty > 0 { t!("无结果") } else { t!("搜索失败") }) + &tail(&notes, "")
}

/// Python 的 f"{x:g}"
pub fn fmt_g(x: f64) -> String {
    let s = format!("{}", (x * 1e6).round() / 1e6);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

// ---------- 设置 ----------
fn price_of(r: &Value) -> Value {
    let usd = if py::truthy(&r["price_usd"]) { r["price_usd"].as_f64() } else { r["price_usd_item"].as_f64().filter(|u| *u != 0.0) };
    if let Some(u) = usd.filter(|u| *u != 0.0) {
        return json!(py::round_n(u * 1000.0 * crate::generation::usd_rate(), 2));
    }
    if r["fields"].as_array().is_some_and(|fs| fs.iter().any(|f| py::truthy(&f["secret"]) && !py::truthy(&f["optional"]))) {
        Value::Null
    } else {
        json!(0)
    }
}

pub fn public() -> Value {
    let mut out = json!({"block": block_list(), "default_block": cfg()["block"].as_array().map(|a| a.iter().map(py::str_of).collect::<Vec<_>>()).unwrap_or_default(),
                         "posts_budget_usd": budget(), "usd_rate": crate::generation::usd_rate()});
    for kind in KINDS {
        let names = chosen(kind);
        let mut rows = vec![];
        for r in recipes(Some(kind)) {
            let why = missing(&r);
            let is_on = on(kind, &r, &names) && (!names.is_empty() || why.is_empty());
            let plat = r["platform"].as_str().unwrap_or("");
            let fields: Vec<Value> = r["fields"].as_array().cloned().unwrap_or_default().iter().map(|f| {
                let l = tr(f, "label");
                json!({"key": f["key"], "label": if l.is_empty() { py::str_of(&f["key"]) } else { l }, "secret": py::truthy(&f["secret"]),
                       "optional": py::truthy(&f["optional"]), "placeholder": f["placeholder"].as_str().unwrap_or(""),
                       "set": !field_value(&r, f).is_empty(), "value": if py::truthy(&f["secret"]) { String::new() } else { field_value(&r, f) }})
            }).collect();
            rows.push(json!({
                "name": r["name"], "title": tr(&r, "title"), "desc": tr(&r, "desc"), "link": r["link"].as_str().unwrap_or(""),
                "platform": if plat.is_empty() { String::new() } else { platform_name(plat) }, "per_item": py::truthy(&r["price_usd_item"]), "site_only": py::truthy(&r["hosts"]),
                "via": r["via"].as_str().unwrap_or(""), "missing": why, "on": is_on, "auto": r.get("auto").cloned().unwrap_or(json!(true)),
                "price": price_of(&r), "provider": provider_of(&r).unwrap_or_default(), "fields": fields}));
        }
        if !names.is_empty() {
            rows.sort_by_key(|x| names.iter().position(|n| x["name"] == n.as_str()).unwrap_or(names.len()));
        }
        out[kind] = json!({"auto": names.is_empty(), "services": rows});
    }
    out
}

pub fn save(data: &Value) -> Result<Value, String> {
    let mut s = saved();
    for kind in KINDS {
        let Some(v) = data.get(kind) else { continue };
        if !py::truthy(v) {
            s.remove(kind);
            s.remove(&format!("{kind}_seen"));
            continue;
        }
        let Some(list) = v.as_array().filter(|a| a.iter().all(|x| x.is_string())) else {
            return Err(t!("顺序要是名字的列表"));
        };
        let names: Vec<String> = list.iter().map(py::str_of).collect();
        if let Some(bad) = names.iter().find(|x| recipe(kind, x).is_none()) {
            return Err(t!("没有这个服务：{name}", name = bad.clone()));
        }
        let mut uniq: Vec<String> = vec![];
        for n in names {
            if !uniq.contains(&n) {
                uniq.push(n);
            }
        }
        s.insert(kind.into(), json!(uniq));
        s.insert(format!("{kind}_seen"), json!(recipes(Some(kind)).iter().map(|r| r["name"].clone()).collect::<Vec<_>>()));
    }
    if let Some(v) = data.get("posts_budget_usd") {
        if v.is_null() {
            s.remove("posts_budget_usd");
        } else {
            let f = match v {
                Value::Number(n) => n.as_f64(),
                Value::String(x) => x.trim().parse::<f64>().ok(),
                Value::Bool(b) => Some(*b as i64 as f64),
                _ => None,
            }
            .ok_or_else(|| t!("预算要是数字"))?;
            if !(0.0..=10.0).contains(&f) {
                return Err(t!("预算在 0 到 10 美元之间"));
            }
            s.insert("posts_budget_usd".into(), json!(f));
        }
    }
    if let Some(v) = data.get("block") {
        if v.is_null() {
            s.remove("block");
        } else {
            let list = v.as_array().ok_or_else(|| t!("不用的网站要是域名的列表"))?;
            let mut ds: Vec<String> = vec![];
            static SCHEME: LazyLock<Regex> = LazyLock::new(|| re(r"^[a-z]+://"));
            for x in list {
                let raw = py::str_of(x);
                let low = raw.trim().to_lowercase();
                let d0 = SCHEME.replace(&low, "").into_owned();
                let mut d = d0.split('/').next().unwrap_or("").trim_matches('.').to_string();
                if let Some(rest) = d.strip_prefix("www.") {
                    d = rest.to_string();
                }
                if !DOMAIN.is_match(&d).unwrap_or(false) {
                    return Err(t!("不是域名：{text}", text = py::head(&raw, 60)));
                }
                if !ds.contains(&d) {
                    ds.push(d);
                }
            }
            s.insert("block".into(), json!(ds));
        }
    }
    if let Some(fields) = data["fields"].as_object() {
        for (name, vals) in fields {
            if crate::security::demo() {
                return Err(t!("演示服务器上不能改搜索服务的密钥和地址"));
            }
            let all = recipes(None);
            let r = all.iter().find(|x| x["name"] == name.as_str());
            let fs: HashMap<String, Value> = all
                .iter()
                .filter(|x| x["name"] == name.as_str())
                .flat_map(|x| x["fields"].as_array().cloned().unwrap_or_default())
                .map(|f| (py::str_of(&f["key"]), f))
                .collect();
            let (Some(_), Some(vals)) = (r, vals.as_object()) else {
                return Err(t!("没有这个服务：{name}", name = name.clone()));
            };
            for (k, val) in vals {
                let f = fs.get(k).ok_or_else(|| t!("没有这一项：{name}", name = k.clone()))?;
                let val = if py::truthy(val) { py::str_of(val).trim().to_string() } else { String::new() };
                if k == "url" && !val.is_empty() && !re(r"^https?://[^\s/]+").is_match(&val) {
                    return Err(t!("地址要以 http:// 或 https:// 开头"));
                }
                if py::truthy(&f["secret"]) && !val.is_empty() {
                    vault::put(&vault_name(name, k), &val)?;
                } else if py::truthy(&f["secret"]) {
                    vault::delete(&vault_name(name, k));
                } else {
                    let fbox = s.entry("fields").or_insert(json!({}));
                    if !fbox.is_object() {
                        *fbox = json!({});
                    }
                    let nb = fbox.as_object_mut().unwrap().entry(name.clone()).or_insert(json!({}));
                    if let Some(o) = nb.as_object_mut() {
                        if val.is_empty() {
                            o.remove(k);
                        } else {
                            o.insert(k.clone(), json!(val));
                        }
                    }
                }
            }
        }
    }
    store(&s);
    Ok(public())
}

pub async fn test(kind: &str, name: &str) -> Result<Value, String> {
    let r = recipe(kind, name).ok_or_else(|| t!("没有这个服务：{name}", name = name))?;
    let why = missing(&r);
    if !why.is_empty() {
        return Ok(json!({"ok": false, "error": why, "ms": 0}));
    }
    let t0 = Instant::now();
    let (ok, out) = if kind == "search" {
        let out = search("DeepSeek", None, Some(&r)).await;
        (out.starts_with("- "), out)
    } else if kind == "posts" {
        let out = search_posts(r["platform"].as_str().unwrap_or(""), "DeepSeek", None, None, Some(&r), true).await;
        (out.starts_with("- ") || r["via"] == "browser", out)
    } else if py::truthy(&r["hosts"]) {
        let u = if py::truthy(&r["test_url"]) { py::str_of(&r["test_url"]) } else { format!("https://{}/", py::str_of(&r["hosts"][0])) };
        let out = fetch(&u, false, Some(&r), true).await;
        (!out.starts_with(&t!("没读到")), out)
    } else {
        let out = fetch("https://example.com/", false, Some(&r), true).await;
        (out.contains("Example Domain"), out)
    };
    let ms = t0.elapsed().as_millis() as i64;
    Ok(
        json!({"ok": ok, "ms": ms, "error": if ok { String::new() } else { py::head(out.trim(), 300) }, "sample": if ok { py::head(&out, 300) } else { String::new() }}),
    )
}
