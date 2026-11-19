//! 手机 App 的推送中转：自己部署的服务器拿不到我们的苹果推送密钥，推送经这里转给苹果（APNs）。对应 osa/relay.py。
//!
//! 单独运行（`osa-relay --host 0.0.0.0 --port 8790`），不碰服务的数据库和 config.yaml。环境变量和 Python 版一样：
//!   OSA_RELAY_ORIGIN  对外地址（校验 VAPID 的 aud、拼订阅地址），默认 https://push.cclsp.top
//!   OSA_RELAY_DB      SQLite 文件，默认 <OSA_ROOT 或当前目录>/data/relay.db
//!   APNS_KEY_FILE、APNS_KEY_ID、APNS_TEAM_ID、APNS_TOPIC
//! 看不到内容；不写访问日志。来源 IP 认 X-Forwarded-For（和 uvicorn --forwarded-allow-ips * 一样）。

use axum::Json;
use axum::extract::{ConnectInfo, Path};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey, signature::Signer, signature::Verifier};
use p256::pkcs8::DecodePrivateKey;
use parking_lot::Mutex;
use regex::Regex;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

pub const MAX_BODY: usize = 2900;
pub const MAX_TTL: i64 = 28 * 86400;
pub const PER_MINUTE: usize = 60;
pub const PER_DAY: u64 = 2000;
pub const IP_PER_MINUTE: usize = 600;
pub const REG_PER_MINUTE: usize = 10;
pub const REG_PER_DAY: u64 = 100;
pub const MAX_PER_TOKEN: i64 = 20;
pub const STALE: f64 = 90.0 * 86400.0;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string())
}

static ORIGIN: LazyLock<String> = LazyLock::new(|| env("OSA_RELAY_ORIGIN", "https://push.cclsp.top").trim_end_matches('/').to_string());
static KEY_FILE: LazyLock<String> = LazyLock::new(|| env("APNS_KEY_FILE", ""));
static KEY_ID: LazyLock<String> = LazyLock::new(|| env("APNS_KEY_ID", ""));
static TEAM_ID: LazyLock<String> = LazyLock::new(|| env("APNS_TEAM_ID", "HMDAVG9B8J"));
static TOPIC: LazyLock<String> = LazyLock::new(|| env("APNS_TOPIC", "com.kadaliao.opensuperagent"));

fn apns_host(env: &str) -> Option<&'static str> {
    match env {
        "production" => Some("https://api.push.apple.com"),
        "sandbox" => Some("https://api.sandbox.push.apple.com"),
        _ => None,
    }
}

static DB: LazyLock<Mutex<Connection>> = LazyLock::new(|| {
    let p = std::env::var_os("OSA_RELAY_DB").filter(|v| !v.is_empty()).map(std::path::PathBuf::from).unwrap_or_else(|| {
        let root =
            std::env::var_os("OSA_ROOT").map(std::path::PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        root.join("data").join("relay.db")
    });
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let c = Connection::open(&p).unwrap_or_else(|e| panic!("打不开 {}：{e}", p.display()));
    let _ = c.busy_timeout(Duration::from_secs(5));
    let _ = c.execute_batch(
        "create table if not exists endpoints(id text primary key, secret_hash text, token text, env text, vapid text,
          created real, last_ok real default 0, fails integer default 0);
         create index if not exists endpoints_token on endpoints(token);",
    );
    Mutex::new(c)
});

fn now() -> f64 {
    crate::py::now()
}

fn b64u(b: &[u8]) -> String {
    crate::util::b64url(b)
}

fn unb64u(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn cleanup() {
    let _ = DB.lock().execute("delete from endpoints where max(created, last_ok) < ?", params![now() - STALE]);
}

// ---------- 限速 ----------
#[derive(Default)]
struct Limiter {
    minute: HashMap<String, Vec<f64>>,
    day: HashMap<String, (i64, u64)>,
}

impl Limiter {
    fn hit(&mut self, key: &str, per_minute: usize, per_day: u64) -> bool {
        let t = now();
        if self.minute.len() > 50000 {
            self.minute.retain(|_, v| v.last().is_some_and(|x| t - x < 60.0));
        }
        let recent: Vec<f64> = self.minute.get(key).map(|v| v.iter().copied().filter(|x| t - x < 60.0).collect()).unwrap_or_default();
        if recent.len() >= per_minute {
            self.minute.insert(key.into(), recent);
            return false;
        }
        if per_day > 0 {
            let today = (t / 86400.0) as i64;
            let (d, n) = self.day.get(key).copied().unwrap_or((today, 0));
            let n = if d == today { n } else { 0 };
            if n >= per_day {
                return false;
            }
            self.day.insert(key.into(), (today, n + 1));
        }
        let mut r = recent;
        r.push(t);
        self.minute.insert(key.into(), r);
        true
    }
}

static LIMITS: LazyLock<Mutex<Limiter>> = LazyLock::new(Default::default);

fn ip(h: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if let Some(f) = h.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(first) = f.split(',').map(|s| s.trim()).find(|s| !s.is_empty())
    {
        return first.to_string();
    }
    peer.map(|p| p.ip().to_string()).unwrap_or_else(|| "?".into())
}

fn err(status: u16, detail: &str) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST), Json(json!({"detail": detail}))).into_response()
}

// ---------- VAPID（RFC 8292） ----------
/// 校验服务器的签名。没问题返回 None，否则返回 (状态码, 原因)
pub fn verify_vapid(header: &str, want_key: &str) -> Option<(u16, &'static str)> {
    static HEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)^\s*vapid\s+(.+)$").unwrap());
    static PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w+)\s*=\s*([^,\s]+)").unwrap());
    let Some(m) = HEAD.captures(header) else { return Some((401, "missing vapid authorization")) };
    let params: HashMap<String, String> = PARAM.captures_iter(&m[1]).map(|c| (c[1].to_string(), c[2].to_string())).collect();
    let token = params.get("t").cloned().unwrap_or_default();
    let k = params.get("k").cloned().unwrap_or_default();
    let parsed = (|| {
        let key = unb64u(&k)?;
        let vk = VerifyingKey::from_sec1_bytes(&key).ok()?;
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        let head: Value = serde_json::from_slice(&unb64u(parts[0])?).ok()?;
        let claims: Value = serde_json::from_slice(&unb64u(parts[1])?).ok()?;
        let sig = unb64u(parts[2])?;
        Some((key, vk, parts[0].to_string(), parts[1].to_string(), head, claims, sig))
    })();
    let Some((key, vk, h64, c64, head, claims, sig)) = parsed else { return Some((401, "malformed vapid token")) };
    let want = unb64u(want_key).unwrap_or_default();
    if key.len() != want.len() || key.iter().zip(&want).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
        return Some((403, "key does not match this endpoint"));
    }
    if head["alg"] != "ES256" || sig.len() != 64 {
        return Some((401, "vapid token must be ES256"));
    }
    let Ok(s) = Signature::from_slice(&sig) else { return Some((401, "bad vapid signature")) };
    if vk.verify(format!("{h64}.{c64}").as_bytes(), &s).is_err() {
        return Some((401, "bad vapid signature"));
    }
    let t = now();
    if claims["aud"] != ORIGIN.as_str() {
        return Some((401, "wrong audience"));
    }
    match claims["exp"].as_f64() {
        Some(e) if claims["exp"].is_number() && t < e && e <= t + 86400.0 => {}
        _ => return Some((401, "vapid token expired or too long-lived")),
    }
    if !crate::py::truthy(&claims["sub"]) {
        return Some((401, "vapid token has no subject"));
    }
    None
}

// ---------- 苹果推送 ----------
struct Apns {
    client: reqwest::Client,
    jwt: Mutex<(String, f64)>,
}

static APNS: LazyLock<Apns> = LazyLock::new(|| Apns {
    client: reqwest::Client::builder().timeout(Duration::from_secs(15)).build().expect("apns client"),
    jwt: Mutex::new((String::new(), 0.0)),
});

impl Apns {
    /// 给苹果的令牌（JWT），40 分钟换一次
    fn token(&self, fresh: bool) -> Option<String> {
        let mut j = self.jwt.lock();
        if fresh || j.0.is_empty() || now() - j.1 > 40.0 * 60.0 {
            let pem = std::fs::read_to_string(&*KEY_FILE).ok()?;
            let key = p256::SecretKey::from_pkcs8_pem(&pem).ok()?;
            let head = b64u(crate::py::dumps(&json!({"alg": "ES256", "kid": *KEY_ID}), true).as_bytes());
            let claims = b64u(crate::py::dumps(&json!({"iss": *TEAM_ID, "iat": now() as i64}), true).as_bytes());
            let sig: Signature = SigningKey::from(&key).sign(format!("{head}.{claims}").as_bytes());
            *j = (format!("{head}.{claims}.{}", b64u(&sig.to_bytes())), now());
        }
        Some(j.0.clone())
    }

    async fn send(&self, device: &str, env: &str, payload: &[u8], headers: &[(String, String)]) -> (u16, String) {
        for attempt in 0..2 {
            let Some(tok) = self.token(attempt > 0) else { return (502, "apns key not configured".into()) };
            let Some(host) = apns_host(env) else { return (502, "bad env".into()) };
            let mut rb = self
                .client
                .post(format!("{host}/3/device/{device}"))
                .body(payload.to_vec())
                .header("authorization", format!("bearer {tok}"));
            for (k, v) in headers {
                rb = rb.header(k.as_str(), v.as_str());
            }
            let r = match rb.send().await {
                Ok(r) => r,
                Err(e) => return (502, if e.is_timeout() { "ReadTimeout".into() } else { "ConnectError".into() }),
            };
            let status = r.status().as_u16();
            let mut reason = String::new();
            if status != 200 {
                reason = r.json::<Value>().await.ok().and_then(|v| v["reason"].as_str().map(String::from)).unwrap_or_default();
            }
            if status == 403 && (reason == "ExpiredProviderToken" || reason == "InvalidProviderToken") && attempt == 0 {
                continue;
            }
            return (status, reason);
        }
        (502, "provider token rejected".into())
    }
}

// ---------- 接口 ----------
fn fields(data: &Value, need: &[&str]) -> Result<serde_json::Map<String, Value>, String> {
    static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64,200}$").unwrap());
    let Some(d) = data.as_object() else { return Err("expected a JSON object".into()) };
    let mut out = serde_json::Map::new();
    if d.contains_key("token") || need.contains(&"token") {
        let t = d
            .get("token")
            .map(|v| if v.is_string() { v.as_str().unwrap().to_string() } else { crate::py::str_of(v) })
            .unwrap_or_default()
            .to_lowercase();
        if !TOKEN.is_match(&t) {
            return Err("bad device token".into());
        }
        out.insert("token".into(), json!(t));
    }
    if d.contains_key("env") || need.contains(&"env") {
        let e = d.get("env").and_then(|v| v.as_str()).unwrap_or("");
        if apns_host(e).is_none() {
            return Err("env must be production or sandbox".into());
        }
        out.insert("env".into(), json!(e));
    }
    if d.contains_key("vapid") || need.contains(&"vapid") {
        let v =
            d.get("vapid").map(|v| if v.is_string() { v.as_str().unwrap().to_string() } else { crate::py::str_of(v) }).unwrap_or_default();
        let raw = unb64u(&v).filter(|r| p256::PublicKey::from_sec1_bytes(r).is_ok()).ok_or_else(|| "bad vapid key".to_string())?;
        out.insert("vapid".into(), json!(b64u(&raw)));
    }
    Ok(out)
}

async fn register(h: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: axum::body::Bytes) -> Response {
    if !LIMITS.lock().hit(&format!("reg:{}", ip(&h, Some(peer))), REG_PER_MINUTE, REG_PER_DAY) {
        return err(429, "too many registrations");
    }
    let data: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(400, &e.to_string()),
    };
    let f = match fields(&data, &["token", "env", "vapid"]) {
        Ok(f) => f,
        Err(e) => return err(400, &e),
    };
    let n: i64 =
        DB.lock().query_row("select count(*) from endpoints where token=?", params![f["token"].as_str()], |r| r.get(0)).unwrap_or(0);
    if n >= MAX_PER_TOKEN {
        return err(429, "too many endpoints for this device");
    }
    cleanup();
    let eid = b64u(&rand::random::<[u8; 16]>());
    let secret = crate::util::token_urlsafe(32);
    let _ = DB.lock().execute(
        "insert into endpoints(id, secret_hash, token, env, vapid, created) values(?,?,?,?,?,?)",
        params![eid, hash(&secret), f["token"].as_str(), f["env"].as_str(), f["vapid"].as_str(), now()],
    );
    (StatusCode::CREATED, Json(json!({"id": eid, "secret": secret, "endpoint": format!("{}/w/{eid}", *ORIGIN)}))).into_response()
}

fn exists(eid: &str) -> bool {
    DB.lock().query_row("select 1 from endpoints where id=?", params![eid], |_| Ok(())).is_ok()
}

fn owned(eid: &str, h: &HeaderMap) -> bool {
    let given = h.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    let given = given.strip_prefix("Bearer ").unwrap_or(given).trim();
    let row: Option<String> = DB.lock().query_row("select secret_hash from endpoints where id=?", params![eid], |r| r.get(0)).ok();
    match row {
        Some(want) if !given.is_empty() => {
            let got = hash(given);
            got.len() == want.len() && got.bytes().zip(want.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
        }
        _ => false,
    }
}

async fn update(Path(eid): Path<String>, h: HeaderMap, body: axum::body::Bytes) -> Response {
    if !exists(&eid) {
        return err(404, "unknown endpoint");
    }
    if !owned(&eid, &h) {
        return err(401, "wrong secret");
    }
    let data: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(400, &e.to_string()),
    };
    let f = match fields(&data, &[]) {
        Ok(f) => f,
        Err(e) => return err(400, &e),
    };
    for (k, v) in f {
        let _ = DB.lock().execute(&format!("update endpoints set {k}=?, fails=0 where id=?"), params![v.as_str(), eid]);
    }
    Json(json!({"id": eid, "endpoint": format!("{}/w/{eid}", *ORIGIN)})).into_response()
}

async fn remove(Path(eid): Path<String>, h: HeaderMap) -> Response {
    if !exists(&eid) {
        return StatusCode::NO_CONTENT.into_response();
    }
    if !owned(&eid, &h) {
        return err(401, "wrong secret");
    }
    let _ = DB.lock().execute("delete from endpoints where id=?", params![eid]);
    StatusCode::NO_CONTENT.into_response()
}

async fn push(Path(eid): Path<String>, h: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: axum::body::Bytes) -> Response {
    let row: Option<(String, String, String)> = DB
        .lock()
        .query_row("select token, env, vapid from endpoints where id=?", params![eid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .ok();
    let Some((device, env, vapid)) = row else { return err(404, "unknown endpoint") };
    let hv = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if let Some((s, why)) = verify_vapid(&hv("authorization"), &vapid) {
        return err(s, why);
    }
    if hv("content-encoding").to_lowercase() != "aes128gcm" {
        return err(400, "content-encoding must be aes128gcm");
    }
    let ttl = match hv("ttl").trim().parse::<i64>() {
        Ok(t) if t >= 0 => t.min(MAX_TTL),
        _ => return err(400, "missing or bad TTL"),
    };
    if hv("content-length").parse::<usize>().unwrap_or(0) > MAX_BODY || body.len() > MAX_BODY {
        return err(413, "payload too large");
    }
    if body.len() < 21 + 65 + 17 || body[20] != 65 {
        return err(400, "not an aes128gcm message");
    }
    {
        let mut l = LIMITS.lock();
        if !l.hit(&format!("ip:{}", ip(&h, Some(peer))), IP_PER_MINUTE, 0) || !l.hit(&format!("ep:{eid}"), PER_MINUTE, PER_DAY) {
            drop(l);
            let mut r = err(429, "too many pushes");
            r.headers_mut().insert("retry-after", "60".parse().unwrap());
            return r;
        }
    }
    let urgency = {
        let u = hv("urgency").to_lowercase();
        if u.is_empty() { "normal".to_string() } else { u }
    };
    let mut headers: Vec<(String, String)> = vec![
        ("apns-push-type".into(), "alert".into()),
        ("apns-topic".into(), TOPIC.clone()),
        ("apns-priority".into(), if urgency == "low" || urgency == "very-low" { "5".into() } else { "10".into() }),
        ("apns-expiration".into(), (if ttl > 0 { now() as i64 + ttl } else { 0 }).to_string()),
    ];
    static TOPIC_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,32}$").unwrap());
    let topic = hv("topic");
    if TOPIC_RE.is_match(&topic) {
        headers.push(("apns-collapse-id".into(), topic));
    }
    let payload = serde_json::to_vec(&json!({"aps": {"alert": {"loc-key": "PUSH_PLACEHOLDER"}, "mutable-content": 1, "sound": "default"},
                                              "e": crate::util::b64(&body), "s": eid}))
    .unwrap_or_default();
    let (mut status, mut reason) = APNS.send(&device, &env, &payload, &headers).await;
    if status == 400 && reason == "BadDeviceToken" {
        let other = if env == "production" { "sandbox" } else { "production" };
        (status, reason) = APNS.send(&device, other, &payload, &headers).await;
        if status == 200 {
            let _ = DB.lock().execute("update endpoints set env=? where id=?", params![other, eid]);
        }
    }
    if status == 200 {
        let _ = DB.lock().execute("update endpoints set last_ok=?, fails=0 where id=?", params![now(), eid]);
        return Response::builder()
            .status(201)
            .header("location", format!("{}/m/{}", *ORIGIN, b64u(&rand::random::<[u8; 12]>())))
            .body(axum::body::Body::empty())
            .unwrap();
    }
    if status == 410 || ["BadDeviceToken", "DeviceTokenNotForTopic", "Unregistered"].contains(&reason.as_str()) {
        let _ = DB.lock().execute("delete from endpoints where id=?", params![eid]);
        return err(410, if reason.is_empty() { "device gone" } else { &reason });
    }
    let _ = DB.lock().execute("update endpoints set fails=fails+1 where id=?", params![eid]);
    if status == 413 || status == 429 {
        return err(status, &reason);
    }
    err(502, format!("apns {status} {reason}").trim())
}

async fn health() -> Response {
    let ok = !KEY_FILE.is_empty() && !KEY_ID.is_empty() && std::path::Path::new(&*KEY_FILE).exists();
    Json(json!({"ok": true, "apns": ok, "http2": true})).into_response()
}

pub fn app() -> axum::Router {
    axum::Router::new()
        .route("/v1/endpoints", post(register))
        .route("/v1/endpoints/{eid}", axum::routing::put(update).merge(delete(remove)))
        .route("/w/{eid}", post(push))
        .route("/healthz", get(health))
}

/// osa-relay 的入口：--host、--port（和 uvicorn 一样的用法）
pub async fn main() -> anyhow::Result<()> {
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8790;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => host = args.next().unwrap_or(host),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(port),
            _ => {}
        }
    }
    LazyLock::force(&DB);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("osa-relay 在 http://{addr} 上运行");
    axum::serve(listener, app().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
