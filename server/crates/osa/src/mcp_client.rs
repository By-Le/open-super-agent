//! MCP 客户端：JSON-RPC 走三种连接方式（stdio、Streamable HTTP、旧版 HTTP + SSE）。
//! Python 版用官方 SDK 的 ClientSession；这里只做 osa 用到的：initialize、tools/list（第一页，和 SDK 的 list_tools() 一样）、
//! tools/call、resources/read。服务端反过来发的请求（ping 回 {}，roots / sampling / elicitation 回“不支持”）。

use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

pub const PROTOCOL: &str = "2025-11-25";
pub const HANDSHAKE: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
pub const CONNECTION_CLOSED: i64 = -32000;

#[derive(Debug, Clone)]
pub enum McpError {
    /// 对方回了 JSON-RPC 错误
    Rpc { code: i64, message: String },
    /// 连接断了
    Closed(String),
    /// 连不上、HTTP 出错
    Transport(String),
    /// 要登录授权（HTTP 401）
    Unauthorized(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpError::Rpc { message, .. } => f.write_str(message),
            McpError::Closed(m) => write!(f, "Connection closed{}", if m.is_empty() { String::new() } else { format!(": {m}") }),
            McpError::Transport(m) => f.write_str(m),
            McpError::Unauthorized(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for McpError {}

impl McpError {
    /// 是不是连接本身断了（而不是工具报错）
    pub fn lost(&self) -> bool {
        match self {
            McpError::Rpc { code, message } => *code == CONNECTION_CLOSED || message.contains("Session terminated"),
            _ => true,
        }
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpError>>>>>;

/// 一条连接
pub struct Session {
    next: AtomicI64,
    pending: Pending,
    out: mpsc::UnboundedSender<Value>,
    closed: watch::Receiver<Option<String>>,
    pub server_info: Mutex<Value>,
    child: Mutex<Option<tokio::process::Child>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
        if let Some(mut c) = self.child.lock().take() {
            let _ = c.start_kill();
        }
    }
}

pub enum Transport {
    Stdio { command: String, args: Vec<String>, env: HashMap<String, String> },
    Http { url: String, headers: HashMap<String, String>, auth: Option<Arc<dyn Auth>> },
    Sse { url: String, headers: HashMap<String, String>, auth: Option<Arc<dyn Auth>> },
}

/// 远程应用的登录授权（OAuth，见 mcp_oauth.rs）：每个请求前给出 Authorization，收到 401 时走一遍授权再重发
pub trait Auth: Send + Sync {
    /// 请求前要带的 Authorization（令牌过期了先刷新）。protocol：已经商定的协议版本
    fn header(&self, protocol: Option<String>) -> futures::future::BoxFuture<'_, Option<String>>;
    /// 收到 401：走完整的授权（可能要等用户在弹窗里登录），成功后调用方重发一次
    fn unauthorized(
        &self,
        www_authenticate: Option<String>,
        protocol: Option<String>,
    ) -> futures::future::BoxFuture<'_, Result<(), McpError>>;
}

fn handle_incoming(msg: Value, pending: &Pending, out: &mpsc::UnboundedSender<Value>) {
    let msgs = match msg {
        Value::Array(a) => a,
        m => vec![m],
    };
    for m in msgs {
        let has_method = m.get("method").is_some();
        let id = m.get("id").cloned().filter(|i| !i.is_null());
        if has_method {
            // 服务端发来的请求
            if let Some(id) = id {
                let method = m["method"].as_str().unwrap_or("");
                let reply = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("{method} not supported")}})
                };
                let _ = out.send(reply);
            }
            continue;
        }
        let Some(id) = id.and_then(|i| i.as_i64().or_else(|| i.as_str().and_then(|s| s.parse().ok()))) else { continue };
        if let Some(tx) = pending.lock().remove(&id) {
            let r = if let Some(e) = m.get("error").filter(|e| !e.is_null()) {
                Err(McpError::Rpc { code: e["code"].as_i64().unwrap_or(0), message: e["message"].as_str().unwrap_or("").to_string() })
            } else {
                Ok(m.get("result").cloned().unwrap_or(json!({})))
            };
            let _ = tx.send(r);
        }
    }
}

fn fail_all(pending: &Pending, why: &str) {
    for (_, tx) in pending.lock().drain() {
        let _ = tx.send(Err(McpError::Rpc {
            code: CONNECTION_CLOSED,
            message: format!("Connection closed{}", if why.is_empty() { String::new() } else { format!(": {why}") }),
        }));
    }
}

/// 解析 SSE：返回 (event, data) 列表，buf 里留着没收完的
fn sse_events(buf: &mut String) -> Vec<(String, String)> {
    let mut out = vec![];
    loop {
        let norm = buf.replace("\r\n", "\n");
        if norm.len() != buf.len() {
            *buf = norm;
        }
        let Some(end) = buf.find("\n\n") else { break };
        let block: String = buf.drain(..end + 2).collect();
        let (mut event, mut data) = (String::from("message"), String::new());
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if !data.is_empty() {
            out.push((event, data));
        }
    }
    out
}

impl Session {
    pub async fn connect(t: Transport, extensions: Value, timeout: Duration) -> Result<Arc<Session>, McpError> {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let (closed_tx, closed_rx) = watch::channel::<Option<String>>(None);
        let mut tasks = vec![];
        let mut child_slot = None;
        match t {
            Transport::Stdio { command, args, env } => {
                let mut cmd = tokio::process::Command::new(&command);
                cmd.args(&args)
                    .env_clear()
                    .envs(env)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::inherit())
                    .kill_on_drop(true);
                #[cfg(unix)]
                cmd.process_group(0);
                let mut child = cmd.spawn().map_err(|e| McpError::Transport(format!("[Errno 2] {e}: '{command}'")))?;
                let mut stdin = child.stdin.take().ok_or_else(|| McpError::Transport("no stdin".into()))?;
                let stdout = child.stdout.take().ok_or_else(|| McpError::Transport("no stdout".into()))?;
                tasks.push(tokio::spawn(async move {
                    while let Some(m) = out_rx.recv().await {
                        let mut line = serde_json::to_string(&m).unwrap_or_default();
                        line.push('\n');
                        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                            break;
                        }
                    }
                }));
                let p = pending.clone();
                let back = out_tx.clone();
                tasks.push(tokio::spawn(async move {
                    let mut lines = BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            handle_incoming(v, &p, &back);
                        }
                    }
                    let _ = closed_tx.send(Some(String::new()));
                    fail_all(&p, "");
                }));
                child_slot = Some(child);
            }
            Transport::Http { url, headers, auth } => {
                let client = http_client(&headers)?;
                let p = pending.clone();
                let back = out_tx.clone();
                let sid: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
                let proto: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
                tasks.push(tokio::spawn(async move {
                    while let Some(m) = out_rx.recv().await {
                        let is_init = m["method"] == "initialize";
                        let id = m["id"].as_i64();
                        let mut tried_auth = false;
                        let r = loop {
                            let mut rb = client
                                .post(&url)
                                .header("accept", "application/json, text/event-stream")
                                .header("content-type", "application/json")
                                .json(&m);
                            if let Some(s) = sid.lock().clone() {
                                rb = rb.header("mcp-session-id", s);
                            }
                            let pv = proto.lock().clone();
                            if let Some(v) = pv.clone() {
                                rb = rb.header("mcp-protocol-version", v);
                            }
                            if let Some(a) = &auth
                                && let Some(h) = a.header(pv.clone()).await
                            {
                                rb = rb.header("authorization", h);
                            }
                            let r = match rb.send().await {
                                Ok(r) => r,
                                Err(e) => break Err(McpError::Transport(e.to_string())),
                            };
                            match &auth {
                                Some(a) if r.status().as_u16() == 401 && !tried_auth => {
                                    tried_auth = true;
                                    let www = r.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).map(String::from);
                                    if let Err(e) = a.unauthorized(www, pv).await {
                                        break Err(e);
                                    }
                                }
                                _ => break Ok(r),
                            }
                        };
                        let r = match r {
                            Ok(r) => r,
                            Err(e) => {
                                if let Some(id) = id
                                    && let Some(tx) = p.lock().remove(&id)
                                {
                                    let _ = tx.send(Err(e));
                                }
                                continue;
                            }
                        };
                        if let Some(s) = r.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                            *sid.lock() = Some(s.to_string());
                        }
                        let status = r.status().as_u16();
                        if status == 202 {
                            continue;
                        }
                        if status == 404 && sid.lock().is_some() {
                            if let Some(id) = id
                                && let Some(tx) = p.lock().remove(&id)
                            {
                                let _ = tx.send(Err(McpError::Rpc { code: CONNECTION_CLOSED, message: "Session terminated".into() }));
                            }
                            continue;
                        }
                        if !(200..300).contains(&status) {
                            let body = r.text().await.unwrap_or_default();
                            let err = if status == 401 {
                                McpError::Unauthorized(format!("401 Unauthorized: {}", crate::py::head(&body, 200)))
                            } else {
                                McpError::Transport(format!("HTTP {status}: {}", crate::py::head(&body, 200)))
                            };
                            if let Some(id) = id
                                && let Some(tx) = p.lock().remove(&id)
                            {
                                let _ = tx.send(Err(err));
                            }
                            continue;
                        }
                        let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
                        if ct.starts_with("text/event-stream") {
                            let p2 = p.clone();
                            let back2 = back.clone();
                            let proto2 = proto.clone();
                            // 一个请求的回应在它自己的事件流里；读完再发下一个会卡住并行的调用，所以另起一个任务读
                            let fut = async move {
                                let mut s = r.bytes_stream();
                                let mut buf = String::new();
                                while let Some(Ok(b)) = s.next().await {
                                    buf.push_str(&String::from_utf8_lossy(&b));
                                    for (_, data) in sse_events(&mut buf) {
                                        if let Ok(v) = serde_json::from_str::<Value>(&data) {
                                            if is_init && let Some(pv) = v["result"]["protocolVersion"].as_str() {
                                                *proto2.lock() = Some(pv.to_string());
                                            }
                                            handle_incoming(v, &p2, &back2);
                                        }
                                    }
                                }
                            };
                            if is_init {
                                fut.await;
                            } else {
                                tokio::spawn(fut);
                            }
                        } else if let Ok(v) = r.json::<Value>().await {
                            if is_init && let Some(pv) = v["result"]["protocolVersion"].as_str() {
                                *proto.lock() = Some(pv.to_string());
                            }
                            handle_incoming(v, &p, &back);
                        }
                    }
                    let _ = closed_tx.send(Some(String::new()));
                }));
            }
            Transport::Sse { url, headers, auth } => {
                let client = http_client(&headers)?;
                let mut tried_auth = false;
                let r = loop {
                    let mut rb = client.get(&url).header("accept", "text/event-stream");
                    if let Some(a) = &auth
                        && let Some(h) = a.header(None).await
                    {
                        rb = rb.header("authorization", h);
                    }
                    let r = rb.send().await.map_err(|e| McpError::Transport(e.to_string()))?;
                    match &auth {
                        Some(a) if r.status().as_u16() == 401 && !tried_auth => {
                            tried_auth = true;
                            let www = r.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).map(String::from);
                            a.unauthorized(www, None).await?;
                        }
                        _ => break r,
                    }
                };
                if r.status().as_u16() == 401 {
                    return Err(McpError::Unauthorized("401 Unauthorized".into()));
                }
                if !r.status().is_success() {
                    return Err(McpError::Transport(format!("HTTP {}", r.status().as_u16())));
                }
                let base = r.url().clone();
                let (ep_tx, ep_rx) = oneshot::channel::<String>();
                let p = pending.clone();
                let back = out_tx.clone();
                tasks.push(tokio::spawn(async move {
                    let mut s = r.bytes_stream();
                    let mut buf = String::new();
                    let mut ep_tx = Some(ep_tx);
                    while let Some(Ok(b)) = s.next().await {
                        buf.push_str(&String::from_utf8_lossy(&b));
                        for (event, data) in sse_events(&mut buf) {
                            if event == "endpoint" {
                                if let (Some(tx), Ok(u)) = (ep_tx.take(), base.join(data.trim())) {
                                    let _ = tx.send(u.to_string());
                                }
                            } else if let Ok(v) = serde_json::from_str::<Value>(&data) {
                                handle_incoming(v, &p, &back);
                            }
                        }
                    }
                    let _ = closed_tx.send(Some(String::new()));
                    fail_all(&p, "");
                }));
                let auth2 = auth.clone();
                let endpoint = tokio::time::timeout(Duration::from_secs(30), ep_rx)
                    .await
                    .map_err(|_| McpError::Transport("SSE 没有给出 endpoint".into()))?
                    .map_err(|_| McpError::Closed(String::new()))?;
                let p = pending.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(m) = out_rx.recv().await {
                        let id = m["id"].as_i64();
                        let mut rb = client.post(&endpoint).json(&m);
                        if let Some(a) = &auth2
                            && let Some(h) = a.header(None).await
                        {
                            rb = rb.header("authorization", h);
                        }
                        match rb.send().await {
                            Ok(r) if r.status().is_success() => {}
                            Ok(r) => {
                                if let Some(tx) = id.and_then(|id| p.lock().remove(&id)) {
                                    let _ = tx.send(Err(McpError::Transport(format!("HTTP {}", r.status().as_u16()))));
                                }
                            }
                            Err(e) => {
                                if let Some(tx) = id.and_then(|id| p.lock().remove(&id)) {
                                    let _ = tx.send(Err(McpError::Transport(e.to_string())));
                                }
                            }
                        }
                    }
                }));
            }
        }
        let s = Arc::new(Session {
            next: AtomicI64::new(0),
            pending,
            out: out_tx,
            closed: closed_rx,
            server_info: Mutex::new(Value::Null),
            child: Mutex::new(child_slot),
            tasks: Mutex::new(tasks),
        });
        let init = tokio::time::timeout(
            timeout,
            s.request("initialize", json!({"protocolVersion": PROTOCOL, "capabilities": {"extensions": extensions}, "clientInfo": {"name": "mcp", "version": "0.1.0"}})),
        )
        .await
        .map_err(|_| McpError::Transport("timeout".into()))??;
        let pv = init["protocolVersion"].as_str().unwrap_or("");
        if !HANDSHAKE.contains(&pv) {
            return Err(McpError::Transport(format!("Unsupported protocol version from the server: {pv}")));
        }
        *s.server_info.lock() = init;
        s.notify("notifications/initialized", json!({}));
        Ok(s)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.closed.borrow().is_some() {
            self.pending.lock().remove(&id);
            return Err(McpError::Rpc { code: CONNECTION_CLOSED, message: "Connection closed".into() });
        }
        self.out
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .map_err(|_| McpError::Closed(String::new()))?;
        rx.await.map_err(|_| McpError::Rpc { code: CONNECTION_CLOSED, message: "Connection closed".into() })?
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.out.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let r = self.request("tools/list", json!({})).await?;
        Ok(r["tools"].as_array().cloned().unwrap_or_default())
    }

    pub async fn call_tool(&self, name: &str, args: &Value) -> Result<Value, McpError> {
        let args = if args.is_object() { args.clone() } else { json!({}) };
        self.request("tools/call", json!({"name": name, "arguments": args})).await
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Value, McpError> {
        self.request("resources/read", json!({"uri": uri})).await
    }

    /// 连接断了时返回
    pub async fn closed(&self) {
        let mut c = self.closed.clone();
        let _ = c.wait_for(|v| v.is_some()).await;
    }

    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    pub async fn shutdown(&self) {
        let child = self.child.lock().take();
        if let Some(mut c) = child {
            let _ = c.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), c.wait()).await;
        }
        for t in self.tasks.lock().drain(..) {
            t.abort();
        }
    }
}

fn http_client(headers: &HashMap<String, String>) -> Result<reqwest::Client, McpError> {
    let mut h = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (reqwest::header::HeaderName::from_bytes(k.as_bytes()), reqwest::header::HeaderValue::from_str(v)) {
            h.insert(k, v);
        }
    }
    reqwest::Client::builder()
        .default_headers(h)
        .connect_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| McpError::Transport(e.to_string()))
}

/// 工具结果 → 给模型的文字
pub fn text_of(result: &Value) -> String {
    let parts: Vec<String> = result["content"]
        .as_array()
        .map(|a| {
            a.iter().map(|c| c["text"].as_str().filter(|t| !t.is_empty()).map(String::from).unwrap_or_else(|| c.to_string())).collect()
        })
        .unwrap_or_default();
    let out = parts.join("\n");
    if crate::py::truthy(&result["isError"]) { format!("工具报错：{out}") } else { out }
}
