//! 接口用的小工具：报错的格式（和 FastAPI 的 HTTPException 一样是 {"detail": …}）、请求体、来源 IP。

use axum::Json;
use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::SocketAddr;

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub detail: Value,
}

impl ApiError {
    pub fn new(status: u16, detail: impl Into<String>) -> ApiError {
        ApiError { status, detail: Value::String(detail.into()) }
    }
    pub fn bad(detail: impl Into<String>) -> ApiError {
        ApiError::new(400, detail)
    }
    pub fn not_found(detail: impl Into<String>) -> ApiError {
        ApiError::new(404, detail)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = StatusCode::from_u16(self.status).unwrap_or(StatusCode::BAD_REQUEST);
        (code, Json(json!({"detail": self.detail}))).into_response()
    }
}

impl From<String> for ApiError {
    fn from(s: String) -> Self {
        ApiError::bad(s)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::new(500, e.to_string())
    }
}

pub type Res = Result<Json<Value>, ApiError>;

pub fn ok(v: Value) -> Res {
    Ok(Json(v))
}

/// 还没迁过来的接口
pub fn todo(what: &str) -> Res {
    Err(ApiError::new(501, format!("Rust 版还没有这个功能：{what}")))
}

/// 请求体是 JSON（没给或者不是 JSON 都当成空对象，和 Body(default={}) 一样）
pub struct Body(pub Value);

impl<S: Send + Sync> axum::extract::FromRequest<S> for Body {
    type Rejection = ApiError;
    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = axum::body::Bytes::from_request(req, state).await.map_err(|e| ApiError::bad(e.to_string()))?;
        if bytes.is_empty() {
            return Ok(Body(json!({})));
        }
        serde_json::from_slice(&bytes).map(Body).map_err(|e| ApiError::new(422, format!("JSON decode error: {e}")))
    }
}

/// 网址参数
pub type Q = Query<HashMap<String, String>>;

pub fn qi(q: &HashMap<String, String>, k: &str) -> Option<i64> {
    q.get(k).and_then(|v| v.trim().parse().ok())
}

pub fn qs<'a>(q: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    q.get(k).map(|s| s.as_str())
}

/// 连过来的地址
pub struct Peer(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Peer(parts.extensions.get::<axum::extract::ConnectInfo<SocketAddr>>().map(|c| c.0)))
    }
}

/// 来源 IP：经本机的反向代理（Caddy）转发的按 X-Forwarded-For（和 uvicorn --proxy-headers 一样）
pub fn client_ip(h: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let trusted = peer.is_some_and(|p| p.ip().is_loopback()) || std::env::var("FORWARDED_ALLOW_IPS").is_ok_and(|v| v.trim() == "*");
    if trusted
        && let Some(f) = h.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(last) = f.split(',').map(|s| s.trim()).rfind(|s| !s.is_empty())
    {
        return last.to_string();
    }
    peer.map(|p| p.ip().to_string()).unwrap_or_else(|| "?".into())
}

/// 请求是不是经过反向代理来的（webhook 用：转发来的不算本机）
pub fn proxied(h: &HeaderMap) -> bool {
    ["x-forwarded-for", "forwarded", "tailscale-user-login"].iter().any(|k| h.contains_key(*k))
}

/// int(x)：接口参数里的数字
pub fn int(v: &Value) -> Result<i64, ApiError> {
    crate::py::to_int(v).ok_or_else(|| ApiError::bad(format!("invalid literal for int() with base 10: {}", crate::py::repr(v))))
}
