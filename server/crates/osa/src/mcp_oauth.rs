//! 远程应用的 OAuth 授权。对应 osa/mcp_hub.py 的 OAuth 部分，加上它用的官方 SDK（mcp.client.auth.OAuthClientProvider）的流程：
//!
//! 收到 401 → 查受保护资源的元数据（RFC 9728）→ 查授权服务器的元数据（RFC 8414 / OIDC）→ 动态注册（RFC 7591，
//! 用户自己建了客户端的不注册）→ 授权码 + PKCE（弹窗里登录，回调带回授权码）→ 换令牌 → 重发请求。令牌过期前先刷新。
//! osa 在 SDK 上改的几处照搬：Slack 的令牌接口（出错也回 200、token_type 是 user）、Notion（Basic 认证时请求体不带 client_id）、
//! 清单写了 scope 就只要这些、授权地址带参数时（腾讯文档）SDK 拼出两个 ?、额外的授权参数（Google 的 access_type）、
//! 重启后刷新前先查令牌地址（flomo）、滴答清单的 issuer 结尾斜杠。
//!
//! 令牌和动态注册得到的客户端整个存成保险库里的一条（OAUTH_<应用名>），JSON 的样子和 Python 版一样：
//! {"tokens": {...}, "expires_at": 时间或 null, "client": {...}}，换版本不用重新登录。

use crate::i18n::Later;
use crate::mcp_client::{Auth, McpError};
use crate::{mcp_hub, py, server, util, vault};
use futures::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::watch;

pub const LOGIN_WAIT_S: u64 = 600;
const LATEST_PROTOCOL: &str = "2026-07-28";
const KNOWN_METHODS: [&str; 4] = ["none", "client_secret_post", "client_secret_basic", "private_key_jwt"];
const DEFAULT_REDIRECT: &str = "http://127.0.0.1/oauth/callback";

// ---------- 存储 ----------
fn load(name: &str) -> Value {
    vault::get(&mcp_hub::oauth_key(name))
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or(json!({}))
}

fn put(name: &str, kv: Value) {
    let merged = py::merge(&load(name), &kv);
    if let Err(e) = vault::put(&mcp_hub::oauth_key(name), &py::dumps(&merged, true)) {
        tracing::error!("存 {name} 的授权出错：{e}");
    }
}

pub fn has_tokens(name: &str) -> bool {
    py::truthy(&load(name)["tokens"])
}

// ---------- 正在进行的授权 ----------
#[derive(Clone)]
struct Code {
    code: String,
    state: String,
    iss: Option<String>,
}

struct Login {
    url: watch::Sender<Option<String>>,
    code: watch::Sender<Option<Result<Code, String>>>,
    task_done: watch::Sender<bool>,
    state: Mutex<String>,
}

static LOGINS: LazyLock<Mutex<HashMap<String, Arc<Login>>>> = LazyLock::new(Default::default);
static STATES: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

fn login_of(name: &str) -> Option<Arc<Login>> {
    LOGINS.lock().get(name).cloned()
}

/// 正在等用户在弹窗里授权（打开了授权页、还没回调）
pub fn waiting_for_user(name: &str) -> bool {
    login_of(name).is_some_and(|w| w.url.borrow().is_some() && w.code.borrow().is_none())
}

/// 这次连接是用户点了“连接”发起的（还没打开授权页）
pub fn interactive(name: &str) -> bool {
    login_of(name).is_some_and(|w| w.url.borrow().is_none())
}

fn fail(w: &Login, msg: String) {
    if w.code.borrow().is_none() {
        let _ = w.code.send(Some(Err(msg)));
    }
}

pub fn cancel_login(name: &str) {
    if let Some(w) = login_of(name) {
        fail(&w, t!("取消了授权"));
    }
}

fn changed() {
    server::broadcast(json!({"type": "apps_changed"}));
}

/// 开始授权。返回 ("url", 授权页地址)；不需要登录就连上了返回 ("done", 状态)
pub async fn login(name: &str, redirect: &str) -> Result<(String, String), String> {
    if mcp_hub::get(name).is_none() {
        return Err(t!("没有应用 {name}", name = name));
    }
    mcp_hub::save(name, &json!({"oauth_redirect": redirect, "enabled": true}))?;
    mcp_hub::disconnect(name, false).await;
    vault::delete(&mcp_hub::oauth_key(name));
    let w = Arc::new(Login {
        url: watch::channel(None).0,
        code: watch::channel(None).0,
        task_done: watch::channel(false).0,
        state: Mutex::new(String::new()),
    });
    LOGINS.lock().insert(name.to_string(), w.clone());
    mcp_hub::mark_needs_login(name, false);
    let (n2, w2) = (name.to_string(), w.clone());
    tokio::spawn(async move {
        let _ = mcp_hub::connect(&n2, (LOGIN_WAIT_S + 60) as f64).await;
        let _ = w2.task_done.send(true);
    });
    let (mut url_rx, mut done_rx) = (w.url.subscribe(), w.task_done.subscribe());
    let _ = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::select! {
            _ = url_rx.wait_for(|u| u.is_some()) => {},
            _ = done_rx.wait_for(|d| *d) => {},
        }
    })
    .await;
    if let Some(u) = w.url.borrow().clone() {
        return Ok(("url".into(), u));
    }
    if *w.task_done.borrow() {
        return Ok(("done".into(), mcp_hub::status_text(name)));
    }
    mcp_hub::disconnect(name, false).await;
    mcp_hub::set_status(name, Later::plain("打不开授权页：应用没有响应"));
    Ok(("done".into(), mcp_hub::status_text(name)))
}

/// OAuth 回调：把授权码交给等着的连接，等它连上。返回应用名
pub async fn finish_login(state: &str, code: &str, iss: &str, error: &str) -> Option<String> {
    let name = STATES.lock().get(state).cloned()?;
    let w = login_of(&name)?;
    if w.code.borrow().is_some() {
        return None;
    }
    if !error.is_empty() {
        fail(&w, t!("授权没有完成：{error}", error = error));
    } else {
        let _ = w.code.send(Some(Ok(Code {
            code: code.into(),
            state: state.into(),
            iss: if iss.is_empty() { None } else { Some(iss.into()) },
        })));
    }
    mcp_hub::set_status(&name, Later::plain("已授权，正在连接…"));
    changed();
    let mut done = w.task_done.subscribe();
    let _ = tokio::time::timeout(Duration::from_secs(60), done.wait_for(|d| *d)).await;
    Some(name)
}

// ---------- URL 小工具 ----------
fn split_url(u: &str) -> (String, String, String) {
    // (scheme, netloc, path)，和 urlparse 一样不做规范化
    let (scheme, rest) = u.split_once("://").unwrap_or(("", u));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = rest[..end].to_string();
    let after = &rest[end..];
    let pend = after.find(['?', '#']).unwrap_or(after.len());
    (scheme.to_string(), netloc, after[..pend].to_string())
}

fn base_of(u: &str) -> String {
    let (s, n, _) = split_url(u);
    format!("{s}://{n}")
}

/// quote_plus（Python 的 urlencode 用的）
fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urlencode(pairs: &[(String, String)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v))).collect::<Vec<_>>().join("&")
}

fn parse_qsl(q: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()
}

pub fn issuers_match(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (shorter, longer) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    longer == format!("{shorter}/") && shorter == base_of(shorter)
}

fn resource_of(server_url: &str) -> String {
    let (s, n, _) = split_url(server_url);
    let rest = &server_url[server_url.find("://").map(|i| i + 3 + n.len()).unwrap_or(0)..];
    let rest = rest.split('#').next().unwrap_or("");
    format!("{}://{}{rest}", s.to_lowercase(), n.to_lowercase())
}

fn resource_allowed(requested: &str, configured: &str) -> bool {
    let (rs, rn, rp) = split_url(requested);
    let (cs, cn, cp) = split_url(configured);
    if rs.to_lowercase() != cs.to_lowercase() || rn.to_lowercase() != cn.to_lowercase() {
        return false;
    }
    let slash = |p: String| if p.ends_with('/') { p } else { format!("{p}/") };
    slash(rp).starts_with(&slash(cp))
}

fn www_field(www: &str, field: &str) -> Option<String> {
    let re = Regex::new(&format!(r#"{field}=(?:"([^"]+)"|([^\s,]+))"#)).ok()?;
    let c = re.captures(www)?;
    c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())
}

fn prm_urls(www_url: Option<String>, server_url: &str) -> Vec<String> {
    let mut urls = vec![];
    if let Some(u) = www_url {
        urls.push(u);
    }
    let (_, _, path) = split_url(server_url);
    let base = base_of(server_url);
    if !path.is_empty() && path != "/" {
        urls.push(format!("{base}/.well-known/oauth-protected-resource{path}"));
    }
    urls.push(format!("{base}/.well-known/oauth-protected-resource"));
    urls
}

fn asm_urls(auth_server: Option<&str>, server_url: &str) -> Vec<String> {
    let Some(a) = auth_server else { return vec![format!("{}/.well-known/oauth-authorization-server", base_of(server_url))] };
    let (_, _, path) = split_url(a);
    let base = base_of(a);
    if !path.is_empty() && path != "/" {
        let p = path.trim_end_matches('/');
        return vec![
            format!("{base}/.well-known/oauth-authorization-server{p}"),
            format!("{base}/.well-known/openid-configuration{p}"),
            format!("{base}{p}/.well-known/openid-configuration"),
        ];
    }
    vec![format!("{base}/.well-known/oauth-authorization-server"), format!("{base}/.well-known/openid-configuration")]
}

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(30)).build().expect("http")
});

struct Resp {
    status: u16,
    body: String,
}

async fn get_meta(url: &str) -> Result<Resp, String> {
    let r = HTTP.get(url).header("mcp-protocol-version", LATEST_PROTOCOL).send().await.map_err(|e| e.to_string())?;
    Ok(Resp { status: r.status().as_u16(), body: r.text().await.unwrap_or_default() })
}

async fn post_form(url: &str, data: &[(String, String)], headers: &[(String, String)]) -> Result<Resp, String> {
    let mut rb = HTTP.post(url).header("content-type", "application/x-www-form-urlencoded").body(urlencode(data));
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    let r = rb.send().await.map_err(|e| e.to_string())?;
    Ok(Resp { status: r.status().as_u16(), body: r.text().await.unwrap_or_default() })
}

fn prm_of(body: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(body).ok()?;
    let ok = v["resource"].is_string()
        && v["authorization_servers"].as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|x| x.is_string()));
    ok.then_some(v)
}

fn asm_of(body: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(body).ok()?;
    let ok = ["issuer", "authorization_endpoint", "token_endpoint"].iter().all(|k| v[*k].is_string());
    ok.then_some(v)
}

/// 令牌接口的回应先整理一下（Slack）：出错也回 200 的当成 400；token_type 不是 Bearer 的改成 Bearer
fn tidy(r: Resp) -> Resp {
    if r.status != 200 {
        return r;
    }
    let Ok(Value::Object(mut d)) = serde_json::from_str::<Value>(&r.body) else { return r };
    if d.get("ok") == Some(&Value::Bool(false)) {
        return Resp { status: 400, body: r.body };
    }
    if let Some(tt) = d.get("token_type").and_then(|v| v.as_str())
        && tt.to_lowercase() != "bearer"
    {
        d.insert("token_type".into(), json!("Bearer"));
        return Resp { status: 200, body: Value::Object(d).to_string() };
    }
    r
}

/// OAuthToken：access_token 必须有，token_type 只认 Bearer
fn token_of(body: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let at = v["access_token"].as_str().ok_or("access_token: Field required")?;
    let tt = match &v["token_type"] {
        Value::Null => "Bearer".to_string(),
        Value::String(s) => {
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        }
        _ => return Err("token_type: Input should be 'Bearer'".into()),
    };
    if tt != "Bearer" {
        return Err("token_type: Input should be 'Bearer'".into());
    }
    let mut o = Map::new();
    o.insert("access_token".into(), json!(at));
    o.insert("token_type".into(), json!("Bearer"));
    if let Some(e) = py::to_int(&v["expires_in"]).filter(|_| !v["expires_in"].is_null()) {
        o.insert("expires_in".into(), json!(e));
    }
    for k in ["scope", "refresh_token"] {
        if let Some(s) = v[k].as_str() {
            o.insert(k.into(), json!(s));
        }
    }
    Ok(Value::Object(o))
}

fn pkce() -> (String, String) {
    use rand::Rng;
    const CH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    let verifier: String = (0..128).map(|_| CH[rng.gen_range(0..CH.len())] as char).collect();
    use sha2::Digest;
    let challenge = util::b64url(&sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

// ---------- 一次连接用的授权 ----------
#[derive(Default)]
struct Ctx {
    initialized: bool,
    prm: Option<Value>,
    asm: Option<Value>,
    auth_server: Option<String>,
    protocol: Option<String>,
    client: Option<Value>,
    tokens: Option<Value>,
    expiry: Option<f64>,
    scope: Option<String>,
}

pub struct Provider {
    name: String,
    server_url: String,
    redirect: String,
    redirect_uris: Vec<String>,
    fixed: Option<Value>,
    scope: Option<String>,
    params: Option<Map<String, Value>>,
    ctx: tokio::sync::Mutex<Ctx>,
}

/// 给一个应用的这次连接做授权（mcp_hub 在连远程应用前调用）
pub fn provider(name: &str, a: &Value, url: &str) -> Result<Arc<dyn Auth>, String> {
    let redirect = a["oauth_redirect"].as_str().unwrap_or("").to_string();
    let redirect_uris = vec![if redirect.is_empty() { DEFAULT_REDIRECT.to_string() } else { redirect.clone() }];
    let o = if a["oauth"].is_object() { a["oauth"].clone() } else { json!({}) };
    let mut fixed = None;
    if py::truthy(&o["client_id"]) {
        let mut missing = vec![];
        let id = mcp_hub::resolve(&o["client_id"], &mut missing);
        let secret = if py::truthy(&o["client_secret"]) { Some(mcp_hub::resolve(&o["client_secret"], &mut missing)) } else { None };
        if !missing.is_empty() {
            return Err(t!("缺少密钥：{names}", names = missing.join(", ")));
        }
        let mut c = json!({"redirect_uris": redirect_uris, "response_types": ["code"], "token_endpoint_auth_method": if secret.is_some() { "client_secret_post" } else { "none" },
                           "grant_types": ["authorization_code", "refresh_token"], "client_id": id});
        if let Some(s) = secret {
            c["client_secret"] = json!(s);
        }
        fixed = Some(c);
    }
    Ok(Arc::new(Provider {
        name: name.into(),
        server_url: url.into(),
        redirect,
        redirect_uris,
        fixed,
        scope: o["scope"].as_str().filter(|s| !s.is_empty()).map(String::from),
        params: o["params"].as_object().cloned(),
        ctx: tokio::sync::Mutex::new(Ctx::default()),
    }))
}

fn err(m: impl Into<String>) -> McpError {
    McpError::Transport(m.into())
}

impl Provider {
    fn client_info(&self) -> Option<Value> {
        if let Some(f) = &self.fixed {
            return Some(f.clone());
        }
        let c = load(&self.name)["client"].clone();
        if !c.is_object() {
            return None;
        }
        if !self.redirect.is_empty() && c["redirect_uris"] != json!([self.redirect]) {
            return None;
        }
        Some(c)
    }

    fn init(&self, c: &mut Ctx) {
        let stored = load(&self.name);
        c.tokens = stored["tokens"].as_object().map(|o| Value::Object(o.clone()));
        c.client = self.client_info();
        c.expiry = None;
        if c.tokens.is_some()
            && let Some(exp) = stored["expires_at"].as_f64()
        {
            c.expiry = Some(exp - 30.0);
        }
        c.initialized = true;
    }

    fn valid(c: &Ctx) -> bool {
        c.tokens.as_ref().is_some_and(|t| t["access_token"].as_str().is_some_and(|s| !s.is_empty()))
            && c.expiry.is_none_or(|e| py::now() <= e)
    }

    fn can_refresh(c: &Ctx) -> bool {
        c.tokens.as_ref().is_some_and(|t| py::truthy(&t["refresh_token"])) && c.client.is_some()
    }

    fn set_tokens(&self, c: &mut Ctx, tok: Value) {
        let exp = tok["expires_in"].as_i64().filter(|e| *e != 0).map(|e| py::now() + e as f64);
        c.expiry = tok["expires_in"].as_i64().map(|e| py::now() + e as f64);
        put(&self.name, json!({"tokens": tok, "expires_at": exp}));
        c.tokens = Some(tok);
    }

    fn include_resource(c: &Ctx) -> bool {
        c.prm.is_some() || c.protocol.as_deref().is_some_and(|p| p >= "2025-06-18")
    }

    fn resource_url(&self, c: &Ctx) -> String {
        let r = resource_of(&self.server_url);
        if let Some(pr) = c.prm.as_ref().and_then(|p| p["resource"].as_str())
            && resource_allowed(&r, pr)
        {
            return pr.to_string();
        }
        r
    }

    fn token_endpoint(&self, c: &Ctx) -> String {
        c.asm
            .as_ref()
            .and_then(|a| a["token_endpoint"].as_str().map(String::from))
            .unwrap_or_else(|| format!("{}/token", base_of(&self.server_url)))
    }

    fn expected_issuer(&self, c: &Ctx) -> String {
        c.auth_server.clone().unwrap_or_else(|| base_of(&self.server_url))
    }

    /// 令牌接口的认证（RFC 6749 2.3.1）。Basic 时请求体里不带 client_id（Notion 会当成两种认证方式）
    fn token_auth(&self, c: &Ctx, mut data: Vec<(String, String)>) -> Result<(Vec<(String, String)>, Vec<(String, String)>), McpError> {
        let mut headers = vec![];
        let Some(ci) = &c.client else { return Ok((data, headers)) };
        let method = ci["token_endpoint_auth_method"].as_str();
        let secret = ci["client_secret"].as_str().filter(|s| !s.is_empty());
        let id = ci["client_id"].as_str().unwrap_or("");
        match (method, secret) {
            (Some("client_secret_basic"), Some(s)) => {
                let cred = format!("{}:{}", util_quote(id), util_quote(s));
                headers.push(("Authorization".into(), format!("Basic {}", util::b64(cred.as_bytes()))));
                data.retain(|(k, _)| k != "client_secret" && k != "client_id");
            }
            (Some("client_secret_post"), Some(s)) => {
                set_pair(&mut data, "client_id", id);
                set_pair(&mut data, "client_secret", s);
            }
            (None, _) => {}
            (Some(m), _) if KNOWN_METHODS.contains(&m) => {}
            (Some(m), _) => return Err(err(format!("Registered client uses unsupported token_endpoint_auth_method '{m}'"))),
        }
        Ok((data, headers))
    }

    /// 和 SDK 授权流程里一样查元数据（刷新令牌前用；osa 加的）
    async fn discover(&self, c: &mut Ctx) -> Result<(), String> {
        for url in prm_urls(None, &self.server_url) {
            let r = get_meta(&url).await?;
            if r.status == 200
                && let Some(prm) = prm_of(&r.body)
            {
                self.validate_resource(&prm).map_err(|e| e.to_string())?;
                c.auth_server = prm["authorization_servers"][0].as_str().map(String::from);
                c.prm = Some(prm);
                break;
            }
        }
        let mut expected = self.expected_issuer(c);
        if let Some(ci) = &c.client
            && !credentials_match(ci, &expected)
        {
            return Ok(());
        }
        for url in asm_urls(c.auth_server.as_deref(), &self.server_url) {
            let r = get_meta(&url).await?;
            let (ok, asm) = asm_response(&r);
            if !ok {
                return Ok(());
            }
            if let Some(asm) = asm {
                let iss = asm["issuer"].as_str().unwrap_or("").to_string();
                if c.auth_server.is_none() && issuers_match(&iss, &expected) {
                    expected = iss.clone();
                }
                validate_issuer(&iss, &expected).map_err(|e| e.to_string())?;
                c.asm = Some(asm);
                return Ok(());
            }
        }
        Ok(())
    }

    fn validate_resource(&self, prm: &Value) -> Result<(), McpError> {
        let Some(pr) = prm["resource"].as_str() else { return Ok(()) };
        let def = resource_of(&self.server_url);
        if !resource_allowed(&def, pr) {
            return Err(err(format!("Protected resource {pr} does not match expected {def}")));
        }
        Ok(())
    }

    async fn refresh(&self, c: &mut Ctx) -> bool {
        if c.asm.is_none()
            && let Err(e) = self.discover(c).await
        {
            tracing::warn!("查找 {} 的令牌地址失败：{e}", self.server_url);
        }
        let (Some(tok), Some(ci)) = (c.tokens.clone(), c.client.clone()) else { return false };
        let mut data = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".into(), py::str_of(&tok["refresh_token"])),
            ("client_id".into(), py::str_of(&ci["client_id"])),
        ];
        if Self::include_resource(c) {
            data.push(("resource".into(), self.resource_url(c)));
        }
        let Ok((data, headers)) = self.token_auth(c, data) else { return false };
        let r = match post_form(&self.token_endpoint(c), &data, &headers).await {
            Ok(r) => tidy(r),
            Err(_) => return false,
        };
        if r.status != 200 {
            tracing::warn!("Token refresh failed: {}", r.status);
            c.tokens = None;
            c.expiry = None;
            return false;
        }
        match token_of(&r.body) {
            Ok(mut new) => {
                if new.get("scope").is_none()
                    && let Some(s) = tok.get("scope").filter(|s| !s.is_null())
                {
                    new["scope"] = s.clone();
                }
                if new.get("refresh_token").is_none()
                    && let Some(s) = tok.get("refresh_token").filter(|s| !s.is_null())
                {
                    new["refresh_token"] = s.clone();
                }
                self.set_tokens(c, new);
                true
            }
            Err(_) => {
                c.tokens = None;
                c.expiry = None;
                false
            }
        }
    }

    /// 打开授权页、等用户在弹窗里登录（osa 的 on_redirect / on_callback）
    async fn redirect_and_wait(&self, auth_url: String) -> Result<Code, McpError> {
        let name = &self.name;
        let w = login_of(name).filter(|w| w.url.borrow().is_none());
        let Some(w) = w else {
            mcp_hub::mark_needs_login(name, true);
            return Err(McpError::Unauthorized(t!("需要重新登录授权")));
        };
        let (head, query) = match auth_url.split_once('?') {
            Some((h, q)) => (h.to_string(), Some(q.replace('?', "&"))),
            None => (auth_url.clone(), None),
        };
        let mut query = query.unwrap_or_default();
        if let Some(p) = &self.params {
            let mut m: IndexMap<String, String> = parse_qsl(&query).into_iter().collect();
            for (k, v) in p {
                m.insert(k.clone(), py::str_of(v));
            }
            query = urlencode(&m.into_iter().collect::<Vec<_>>());
        }
        let url = if auth_url.contains('?') { format!("{head}?{query}") } else { head };
        let state = parse_qsl(url.split_once('?').map(|x| x.1).unwrap_or(""))
            .into_iter()
            .find(|(k, _)| k == "state")
            .map(|x| x.1)
            .unwrap_or_default();
        STATES.lock().insert(state.clone(), name.clone());
        *w.state.lock() = state.clone();
        mcp_hub::set_status(name, Later::plain("等你在弹出的窗口里登录授权…"));
        let _ = w.url.send(Some(url));
        changed();
        let mut rx = w.code.subscribe();
        let got = tokio::time::timeout(Duration::from_secs(LOGIN_WAIT_S), rx.wait_for(|c| c.is_some())).await;
        STATES.lock().remove(&state);
        match got {
            Err(_) => {
                mcp_hub::mark_needs_login(name, true);
                Err(McpError::Unauthorized(t!("没有完成授权")))
            }
            Ok(Err(_)) => Err(McpError::Unauthorized(t!("没有完成授权"))),
            Ok(Ok(v)) => match v.clone() {
                Some(Ok(code)) => Ok(code),
                Some(Err(msg)) => {
                    mcp_hub::mark_needs_login(name, true);
                    Err(McpError::Unauthorized(msg))
                }
                None => Err(McpError::Unauthorized(t!("没有完成授权"))),
            },
        }
    }

    /// 收到 401 后的完整授权（SDK 的 _auth_flow 第 1–5 步）
    async fn full_flow(&self, c: &mut Ctx, www: Option<String>) -> Result<(), McpError> {
        // 第 1 步：受保护资源的元数据
        let www_url = www.as_deref().and_then(|w| www_field(w, "resource_metadata"));
        let mut failed: Option<u16> = None;
        let mut found = false;
        for url in prm_urls(www_url, &self.server_url) {
            let r = get_meta(&url).await.map_err(err)?;
            if r.status >= 500 || r.status == 429 {
                failed = Some(r.status);
            }
            if r.status == 200
                && let Some(prm) = prm_of(&r.body)
            {
                self.validate_resource(&prm)?;
                c.auth_server = prm["authorization_servers"][0].as_str().map(String::from);
                c.prm = Some(prm);
                found = true;
                break;
            }
        }
        if !found && let Some(code) = failed {
            return Err(err(format!("Protected resource metadata request failed: HTTP {code}")));
        }
        let mut expected = self.expected_issuer(c);
        if c.client.as_ref().is_some_and(|ci| !credentials_match(ci, &expected)) {
            c.client = None;
            c.tokens = None;
            c.expiry = None;
            c.asm = None;
        }
        // 第 2 步：授权服务器的元数据
        for url in asm_urls(c.auth_server.as_deref(), &self.server_url) {
            let r = get_meta(&url).await.map_err(err)?;
            let (ok, asm) = asm_response(&r);
            if !ok {
                break;
            }
            if let Some(asm) = asm {
                let iss = asm["issuer"].as_str().unwrap_or("").to_string();
                if c.auth_server.is_none() && issuers_match(&iss, &expected) {
                    expected = iss.clone();
                }
                validate_issuer(&iss, &expected)?;
                c.asm = Some(asm);
                break;
            }
        }
        // 第 3 步：申请哪些权限
        c.scope = select_scopes(www.as_deref().and_then(|w| www_field(w, "scope")), c.prm.as_ref(), c.asm.as_ref());
        // 第 4 步：动态注册
        if c.client.is_none() {
            let discovered = c.asm.as_ref().map(|_| self.expected_issuer(c));
            let fallback = base_of(&self.server_url);
            let reg_url = c
                .asm
                .as_ref()
                .and_then(|a| a["registration_endpoint"].as_str().map(String::from))
                .unwrap_or_else(|| format!("{fallback}/register"));
            let mut body = Map::new();
            body.insert("response_types".into(), json!(["code"]));
            if let Some(s) = &c.scope {
                body.insert("scope".into(), json!(s));
            }
            body.insert("client_name".into(), json!("Open Super Agent"));
            body.insert("redirect_uris".into(), json!(self.redirect_uris));
            body.insert("grant_types".into(), json!(["authorization_code", "refresh_token"]));
            body.insert("application_type".into(), json!("native"));
            let r = HTTP.post(&reg_url).json(&Value::Object(body)).send().await.map_err(|e| err(e.to_string()))?;
            let status = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            if status != 200 && status != 201 {
                return Err(err(format!("Registration failed: {status} {text}")));
            }
            let mut info: Map<String, Value> = match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(o)) => o.into_iter().filter(|(_, v)| !v.is_null() && v != &json!("")).collect(),
                _ => return Err(err("Invalid registration response")),
            };
            info.remove("issuer");
            if !info.get("client_id").is_some_and(|v| v.is_string()) {
                return Err(err("Invalid registration response: client_id Field required"));
            }
            info.entry("response_types").or_insert(json!(["code"]));
            info.entry("grant_types").or_insert(json!(["authorization_code", "refresh_token"]));
            let method = info.get("token_endpoint_auth_method").and_then(|v| v.as_str()).map(String::from);
            if let Some(m) = &method {
                if !KNOWN_METHODS.contains(&m.as_str()) || m == "private_key_jwt" {
                    return Err(err(format!(
                        "Authorization server registered the client with unsupported token_endpoint_auth_method '{m}'"
                    )));
                }
                if (m == "client_secret_post" || m == "client_secret_basic") && !info.contains_key("client_secret") {
                    return Err(err(format!("Authorization server registered the client for '{m}' but issued no client_secret")));
                }
            }
            if let (Some(asm), Some(d)) = (&c.asm, &discovered)
                && (asm["registration_endpoint"].is_string() || base_of(d) == fallback)
            {
                info.insert("issuer".into(), json!(d));
            }
            let info = Value::Object(info);
            if self.fixed.is_none() {
                put(&self.name, json!({"client": info}));
            }
            c.client = Some(info);
        }
        // 第 5 步：授权码 + 换令牌。清单写了 scope 就只要这些（osa 加的）
        if let Some(s) = &self.scope {
            c.scope = Some(s.clone());
        }
        let auth_endpoint = c
            .asm
            .as_ref()
            .and_then(|a| a["authorization_endpoint"].as_str().map(String::from))
            .unwrap_or_else(|| format!("{}/authorize", base_of(&self.server_url)));
        let ci = c.client.clone().unwrap_or(json!({}));
        let (verifier, challenge) = pkce();
        let state = util::token_urlsafe(32);
        let mut params = vec![
            ("response_type".to_string(), "code".to_string()),
            ("client_id".into(), py::str_of(&ci["client_id"])),
            ("redirect_uri".into(), self.redirect_uris[0].clone()),
            ("state".into(), state.clone()),
            ("code_challenge".into(), challenge),
            ("code_challenge_method".into(), "S256".into()),
        ];
        if Self::include_resource(c) {
            params.push(("resource".into(), self.resource_url(c)));
        }
        if let Some(s) = &c.scope {
            params.push(("scope".into(), s.clone()));
            if s.split_whitespace().any(|x| x == "offline_access") {
                params.push(("prompt".into(), "consent".into()));
            }
        }
        let code = self.redirect_and_wait(format!("{auth_endpoint}?{}", urlencode(&params))).await?;
        if code.state != state {
            return Err(err(format!("State parameter mismatch: {} != {state}", code.state)));
        }
        let expected_iss = c.asm.as_ref().and_then(|a| a["issuer"].as_str().map(String::from));
        match &code.iss {
            Some(i) if Some(i) != expected_iss.as_ref() => {
                return Err(err(format!("Authorization response iss mismatch: {i} != {}", expected_iss.unwrap_or_else(|| "None".into()))));
            }
            None if c.asm.as_ref().is_some_and(|a| a["authorization_response_iss_parameter_supported"] == true) => {
                return Err(err("Authorization response missing iss parameter advertised by the authorization server"));
            }
            _ => {}
        }
        if code.code.is_empty() {
            return Err(err("No authorization code received"));
        }
        let mut data = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".into(), code.code.clone()),
            ("redirect_uri".into(), self.redirect_uris[0].clone()),
            ("client_id".into(), py::str_of(&ci["client_id"])),
            ("code_verifier".into(), verifier),
        ];
        if Self::include_resource(c) {
            data.push(("resource".into(), self.resource_url(c)));
        }
        let (data, headers) = self.token_auth(c, data)?;
        let r = tidy(post_form(&self.token_endpoint(c), &data, &headers).await.map_err(err)?);
        if r.status != 200 && r.status != 201 {
            return Err(err(format!("Token exchange failed ({}): {}", r.status, r.body)));
        }
        let mut tok = token_of(&r.body).map_err(|e| err(format!("Invalid token response: {e}")))?;
        if tok.get("scope").is_none()
            && let Some(s) = &c.scope
        {
            tok["scope"] = json!(s);
        }
        self.set_tokens(c, tok);
        Ok(())
    }
}

fn util_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn set_pair(data: &mut Vec<(String, String)>, k: &str, v: &str) {
    match data.iter_mut().find(|(x, _)| x == k) {
        Some(p) => p.1 = v.to_string(),
        None => data.push((k.into(), v.into())),
    }
}

fn credentials_match(ci: &Value, issuer: &str) -> bool {
    match ci["issuer"].as_str() {
        None => true,
        Some(i) => issuers_match(i, issuer),
    }
}

/// osa 改过的 validate_metadata_issuer：结尾斜杠不同也认（滴答清单）
fn validate_issuer(iss: &str, expected: &str) -> Result<(), McpError> {
    if issuers_match(iss, expected) {
        return Ok(());
    }
    Err(err(format!("Authorization server metadata issuer mismatch: {iss} != {expected}")))
}

fn asm_response(r: &Resp) -> (bool, Option<Value>) {
    if r.status == 200 {
        return (true, asm_of(&r.body));
    }
    if (300..500).contains(&r.status) {
        return (true, None);
    }
    (false, None)
}

fn select_scopes(www: Option<String>, prm: Option<&Value>, asm: Option<&Value>) -> Option<String> {
    let join = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "));
    let mut sel = www.or_else(|| prm.and_then(|p| join(&p["scopes_supported"]))).or_else(|| asm.and_then(|a| join(&a["scopes_supported"])));
    if let (Some(s), Some(a)) = (&sel, asm) {
        let offline = a["scopes_supported"].as_array().is_some_and(|x| x.iter().any(|v| v == "offline_access"));
        if offline && !s.split_whitespace().any(|x| x == "offline_access") {
            sel = Some(format!("{s} offline_access"));
        }
    }
    sel
}

impl Auth for Provider {
    fn header(&self, protocol: Option<String>) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let mut c = self.ctx.lock().await;
            if !c.initialized {
                self.init(&mut c);
            }
            c.protocol = protocol;
            if !Self::valid(&c) && Self::can_refresh(&c) && !self.refresh(&mut c).await {
                c.initialized = false;
            }
            if Self::valid(&c) {
                return c.tokens.as_ref().and_then(|t| t["access_token"].as_str()).map(|t| format!("Bearer {t}"));
            }
            None
        })
    }

    fn unauthorized(&self, www: Option<String>, protocol: Option<String>) -> BoxFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            let mut c = self.ctx.lock().await;
            if !c.initialized {
                self.init(&mut c);
            }
            if protocol.is_some() {
                c.protocol = protocol;
            }
            let r = self.full_flow(&mut c, www).await;
            if let Err(e) = &r {
                tracing::warn!("{} 的 OAuth 授权出错：{e}", self.name);
            }
            r
        })
    }
}
