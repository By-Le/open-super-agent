//! 登录鉴权。设置了环境变量 OSA_PASSWORD 才生效。cookie 的格式、签名和 Python 版一样，换版本不用重新登录。
//! 对应 osa/auth.py（规则见那里的说明）。

use crate::web::{ApiError, Res};
use crate::{config, i18n, py, util};
use axum::body::Body;
use axum::extract::{Query, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::LazyLock;

pub const MAX_AGE: i64 = 30 * 86400;
pub const RENEW_BELOW: i64 = 15 * 86400;
pub const FAIL_WINDOW: f64 = 600.0;
pub const MAX_FAILS: usize = 5;
pub const TICKET_TTL: i64 = 30;
pub const PAIR_TTL: i64 = 300;

const PUBLIC_FILES: [&str; 6] = ["/manifest.json", "/sw.js", "/favicon.png", "/icon-192.png", "/icon-512.png", "/apple-touch-icon.png"];
const PUBLIC_PREFIXES: [&str; 8] = ["/assets/", "/hooks/", "/api/shared/", "/s/", "/skill/", "/template/", "/avatars/", "/pub/"];
const LOCAL_SUFFIXES: [&str; 3] = [".localhost", ".local", ".ts.net"];

pub static COOKIE: LazyLock<String> = LazyLock::new(|| {
    let c = std::env::var("OSA_COOKIE").unwrap_or_default();
    if !c.is_empty() && c.len() <= 64 && c.chars().all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '-') { c } else { "osa_session".into() }
});

static FAILS: LazyLock<Mutex<HashMap<String, Vec<f64>>>> = LazyLock::new(Default::default);
static TICKETS: LazyLock<Mutex<HashMap<String, f64>>> = LazyLock::new(Default::default);
static KEY: LazyLock<Vec<u8>> = LazyLock::new(|| {
    let f = config::DB_PATH.parent().map(|d| d.join("session.key")).unwrap_or_else(|| "session.key".into());
    if !f.exists() {
        let _ = std::fs::write(&f, util::token_hex(32));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600));
        }
    }
    std::fs::read_to_string(&f).unwrap_or_default().trim().as_bytes().to_vec()
});

pub fn enabled() -> bool {
    std::env::var("OSA_PASSWORD").is_ok_and(|p| !p.is_empty())
}

fn signing_key() -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(&KEY).expect("hmac");
    m.update(std::env::var("OSA_PASSWORD").unwrap_or_default().as_bytes());
    m.finalize().into_bytes().to_vec()
}

fn sign(exp: i64) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(&signing_key()).expect("hmac");
    m.update(format!("v1.{exp}").as_bytes());
    m.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_token() -> String {
    let exp = py::now() as i64 + MAX_AGE;
    format!("{exp}.{}", sign(exp))
}

fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn check_token(token: Option<&str>) -> Option<i64> {
    let (exp_s, sig) = token?.split_once('.')?;
    let exp: i64 = exp_s.parse().ok()?;
    if (exp as f64) < py::now() || !eq(sig.as_bytes(), sign(exp).as_bytes()) {
        return None;
    }
    Some(exp)
}

pub fn check_password(given: &str) -> bool {
    use sha2::Digest;
    let want = std::env::var("OSA_PASSWORD").unwrap_or_default();
    !want.is_empty() && eq(&Sha256::digest(given.as_bytes()), &Sha256::digest(want.as_bytes()))
}

fn cookie_header(token: &str, max_age: i64, secure: bool) -> String {
    format!("{}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}", *COOKIE, if secure { "; Secure" } else { "" })
}

pub fn cookies(h: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for v in h.get_all(header::COOKIE) {
        for part in v.to_str().unwrap_or("").split(';') {
            if let Some((k, v)) = part.trim().split_once('=') {
                if !k.is_empty() {
                    out.insert(k.to_string(), v.to_string());
                }
            } else if !part.trim().is_empty() {
                out.insert(part.trim().to_string(), String::new());
            }
        }
    }
    out
}

fn hdr<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok())
}

fn same_origin(h: &HeaderMap) -> bool {
    let Some(origin) = hdr(h, "origin") else { return true };
    if origin == "null" {
        return false;
    }
    let netloc = origin.split_once("://").map(|x| x.1).unwrap_or("").split('/').next().unwrap_or("");
    Some(netloc) == hdr(h, "host")
}

fn allowed_hosts() -> Vec<String> {
    let mut names: Vec<String> = config::get("server.allowed_hosts").as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default();
    names.extend(std::env::var("OSA_ALLOWED_HOSTS").unwrap_or_default().split(',').map(String::from));
    names.into_iter().map(|h| h.trim().to_lowercase()).filter(|h| !h.is_empty()).collect()
}

pub fn host_name(host: Option<&str>) -> String {
    let h = host.unwrap_or("");
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("").to_string()
    } else {
        h.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(n, _)| n.to_string()).unwrap_or_else(|| h.to_string())
    };
    name.to_lowercase().trim_end_matches('.').to_string()
}

pub fn host_ok(host: Option<&str>) -> bool {
    let Some(h) = host.filter(|h| !h.is_empty()) else { return true };
    let name = host_name(Some(h));
    if name.is_empty() {
        return false;
    }
    if name == "localhost" || LOCAL_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    if name.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    allowed_hosts().iter().any(|p| util::fnmatch(&name, p))
}

fn is_public(path: &str) -> bool {
    if path.contains("..") {
        return false;
    }
    PUBLIC_FILES.contains(&path) || ["/login", "/logout", "/login/ticket", "/api/computer/link"].contains(&path) || PUBLIC_PREFIXES.iter().any(|p| path.starts_with(p))
}

fn safe_next(n: Option<&str>) -> String {
    let n = n.filter(|s| !s.is_empty()).unwrap_or("/");
    if !n.starts_with('/') || n.starts_with("//") || n.contains('\\') || n.starts_with("/login") { "/".into() } else { n.to_string() }
}

fn deny(status: StatusCode, detail: String, text: bool) -> Response {
    if text {
        (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], detail).into_response()
    } else {
        (status, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], py::dumps_u(&json!({"detail": detail}))).into_response()
    }
}

fn redirect(location: &str) -> Response {
    Response::builder().status(303).header(header::LOCATION, location).header(header::CONTENT_LENGTH, "0").body(Body::empty()).unwrap()
}

fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn is_ws(req: &Request) -> bool {
    req.headers().get(header::UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

pub fn https(h: &HeaderMap) -> bool {
    hdr(h, "x-forwarded-proto").is_some_and(|p| p.split(',').next().unwrap_or("").trim() == "https")
}

/// HTTP 和 WebSocket 都经过这里
pub async fn middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let ws = is_ws(&req);
    let h = req.headers().clone();
    if (ws || !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")) && !same_origin(&h) {
        return deny(StatusCode::FORBIDDEN, t!("跨站请求被拒绝"), false);
    }
    if !enabled() {
        let hook = path.starts_with("/hooks/") && (std::env::var("OSA_WEBHOOK_TOKEN").is_ok_and(|t| !t.is_empty()) || path.len() - "/hooks/".len() >= 16);
        if !hook && !host_ok(hdr(&h, "host")) {
            return deny(
                StatusCode::FORBIDDEN,
                t!("没有放行这个地址（{host}）：没设登录密码时，只接受本机、IP 地址和 Tailscale 的地址。要用这个地址访问，请设置 OSA_PASSWORD 打开登录，或者把它加进 config.yaml 的 server.allowed_hosts", host = host_name(hdr(&h, "host"))),
                !path.starts_with("/api/"),
            );
        }
        return next.run(req).await;
    }
    if is_public(&path) {
        return next.run(req).await;
    }
    let exp = check_token(cookies(&h).get(&*COOKIE).map(|s| s.as_str()));
    let Some(exp) = exp else {
        if ws {
            return deny(StatusCode::FORBIDDEN, t!("请先登录"), false);
        }
        if (method == "GET" || method == "HEAD") && !path.starts_with("/api/") {
            let q = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
            let nxt = format!("{path}{q}");
            return redirect(&format!("/login{}", if nxt == "/" { String::new() } else { format!("?next={}", quote(&nxt)) }));
        }
        return deny(StatusCode::UNAUTHORIZED, t!("请先登录"), false);
    };
    if !ws && (exp as f64) - py::now() < RENEW_BELOW as f64 {
        let secure = https(&h);
        let mut r = next.run(req).await;
        if let Ok(v) = HeaderValue::from_str(&cookie_header(&new_token(), MAX_AGE, secure)) {
            r.headers_mut().append(header::SET_COOKIE, v);
        }
        return r;
    }
    next.run(req).await
}

fn blocked(ip: &str) -> bool {
    let now = py::now();
    let mut f = FAILS.lock();
    if f.len() > 10000 {
        f.retain(|_, v| v.last().is_some_and(|t| now - t <= FAIL_WINDOW));
    }
    let recent: Vec<f64> = f.get(ip).map(|v| v.iter().copied().filter(|t| now - t < FAIL_WINDOW).collect()).unwrap_or_default();
    let n = recent.len();
    f.insert(ip.to_string(), recent);
    n >= MAX_FAILS
}

fn fail(ip: &str) {
    FAILS.lock().entry(ip.to_string()).or_default().push(py::now());
}

fn lang_of(h: &HeaderMap) -> String {
    let c = i18n::chosen();
    if c.is_empty() { i18n::from_header(hdr(h, "accept-language")) } else { c }
}

fn escape(s: &str, quote: bool) -> String {
    let mut o = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    if quote {
        o = o.replace('"', "&quot;").replace('\'', "&#x27;");
    }
    o
}

fn page(h: &HeaderMap, nxt: &str, error: &str, status: u16) -> Response {
    let lang = lang_of(h);
    let err = if error.is_empty() { String::new() } else { format!("<p class=\"err\">{}</p>", escape(error, true)) };
    let body = include_str!("prompts/login.html")
        .replace("{next}", &escape(nxt, true))
        .replace("{error}", &err)
        .replace("{lang}", if lang == "en" { "en" } else { "zh-CN" })
        .replace("{login}", &i18n::t_in("登录", Some(&lang), &[]))
        .replace("{password}", &i18n::t_in("密码", Some(&lang), &[]));
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .unwrap()
}

pub async fn login_page(h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let nxt = safe_next(q.get("next").map(|s| s.as_str()));
    if !enabled() || check_token(cookies(&h).get(&*COOKIE).map(|s| s.as_str())).is_some() {
        return redirect(&nxt);
    }
    page(&h, &nxt, "", 200)
}

fn client_ip(h: &HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    crate::web::client_ip(h, peer)
}

pub async fn login(h: HeaderMap, peer: crate::web::Peer, body: String) -> Response {
    let form: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().fold(HashMap::new(), |mut m, (k, v)| {
        m.entry(k).or_insert(v);
        m
    });
    let nxt = safe_next(form.get("next").map(|s| s.as_str()));
    if !enabled() {
        return redirect(&nxt);
    }
    let ip = client_ip(&h, peer.0);
    if blocked(&ip) {
        return page(&h, &nxt, &i18n::t_in("输错次数太多，请 10 分钟后再试", Some(&lang_of(&h)), &[]), 429);
    }
    if !check_password(form.get("password").map(|s| s.as_str()).unwrap_or("")) {
        fail(&ip);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return page(&h, &nxt, &i18n::t_in("密码不对", Some(&lang_of(&h)), &[]), 401);
    }
    FAILS.lock().remove(&ip);
    let mut r = redirect(&nxt);
    if let Ok(v) = HeaderValue::from_str(&cookie_header(&new_token(), MAX_AGE, https(&h))) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

pub async fn login_ticket(h: HeaderMap, peer: crate::web::Peer) -> Res {
    if !enabled() {
        return Err(ApiError::bad(t!("没有开登录")));
    }
    let ip = client_ip(&h, peer.0);
    if blocked(&ip) {
        return Err(ApiError::new(429, t!("输错次数太多，请 10 分钟后再试")));
    }
    let given = hdr(&h, "authorization").unwrap_or("");
    let given = given.strip_prefix("Bearer ").unwrap_or(given).trim();
    if !check_password(given) {
        fail(&ip);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        return Err(ApiError::new(401, t!("密码不对")));
    }
    FAILS.lock().remove(&ip);
    Ok(axum::Json(json!({"ticket": mint_ticket(TICKET_TTL), "expires_in": TICKET_TTL})))
}

pub fn mint_ticket(ttl: i64) -> String {
    let now = py::now();
    let mut t = TICKETS.lock();
    t.retain(|_, exp| *exp >= now);
    let ticket = util::token_urlsafe(32);
    t.insert(ticket.clone(), now + ttl as f64);
    ticket
}

pub async fn pair() -> Res {
    if !enabled() {
        return Ok(axum::Json(json!({"ticket": "", "expires_in": 0})));
    }
    Ok(axum::Json(json!({"ticket": mint_ticket(PAIR_TTL), "expires_in": PAIR_TTL})))
}

pub async fn use_ticket(h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let nxt = safe_next(q.get("next").map(|s| s.as_str()));
    if !enabled() {
        return redirect(&nxt);
    }
    let exp = TICKETS.lock().remove(q.get("t").map(|s| s.as_str()).unwrap_or("")).unwrap_or(0.0);
    if exp < py::now() {
        return redirect(&format!("/login{}", if nxt == "/" { String::new() } else { format!("?next={}", quote(&nxt)) }));
    }
    let mut r = redirect(&nxt);
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&cookie_header(&new_token(), MAX_AGE, https(&h))) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

pub async fn logout(h: HeaderMap) -> Response {
    let mut r = redirect(if enabled() { "/login" } else { "/" });
    if let Ok(v) = HeaderValue::from_str(&cookie_header("", 0, https(&h))) {
        r.headers_mut().append(header::SET_COOKIE, v);
    }
    r
}

pub async fn me() -> Res {
    Ok(axum::Json(json!({"auth": enabled()})))
}

#[allow(dead_code)]
fn _v(_: Value) {}
