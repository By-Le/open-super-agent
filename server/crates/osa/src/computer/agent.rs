//! 你的 Mac 上跑的小程序（osa/computer/agent.py）：桌面 App 在你同意“允许派任务到这台 Mac”后在后台启动它（`osa-server agent`），
//! 它主动连到服务器的 /api/computer/link，替派到这台 Mac 上的任务操作它。动手的代码就是本机电脑的（host.rs 的 Place::Local）。
//! 线上的消息和 Python 版一样，旧版服务器、新版服务器都能连。
//!
//! 环境变量：OSA_LINK_URL（wss://服务器/api/computer/link）、OSA_LINK_TOKEN、OSA_DATA_DIR、OSA_PARENT_PID。

use super::base::{Computer, ComputerError, R};
use super::host::{Host, Place};
use super::macdesk;
use crate::{config, py, util};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const MAX_MSG: usize = 64 << 20;
pub const MAX_READ: u64 = 16 << 20;

fn retry(code: u16) -> Option<u64> {
    match code {
        4401 => Some(300),
        4409 => Some(60),
        _ => None,
    }
}

pub fn computer_name() -> String {
    let n = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if !n.is_empty() {
        return n;
    }
    let mut buf = [0u8; 256];
    #[cfg(unix)]
    unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    String::new()
}

/// 这台 Mac 的编号：硬件 UUID 的 SHA-256（不把 UUID 本身发出去）
pub fn device_id() -> String {
    let out = std::process::Command::new("ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output();
    let Ok(out) = out else { return String::new() };
    let text = String::from_utf8_lossy(&out.stdout);
    let re = regex::Regex::new(r#""IOPlatformUUID"\s*=\s*"([0-9A-Fa-f-]{20,})""#).unwrap();
    match re.captures(&text) {
        Some(c) => py::head(&util::sha256_hex(format!("osa-mac:{}", c[1].to_uppercase()).as_bytes()), 32),
        None => String::new(),
    }
}

fn s(a: &Value, k: &str) -> String {
    match &a[k] {
        Value::Null => String::new(),
        v => py::str_of(v),
    }
}

/// 做服务器要的一件事（服务器那边对应的调用见 host.rs 的 Place::Mac）
async fn handle(pc: &Arc<Computer>, h: &Host, op: &str, a: &Value) -> Result<Value, (String, bool)> {
    let ce = |e: ComputerError| (e.0, false);
    match op {
        "helper" => {
            let mut payload = a["payload"].clone();
            if payload["op"] == "upload" {
                let files: Vec<String> = payload["files"].as_array().map(|f| f.iter().map(py::str_of).collect()).unwrap_or_default();
                payload["files"] = json!(h.upload_local(files).map_err(ce)?);
            }
            h.helper_local(pc, &payload).await.map_err(ce)
        }
        "status" => Ok(pc.status().await),
        "start" => pc.start().await.map(|_| Value::Null).map_err(ce),
        "pause" => pc.pause().await.map(|_| Value::Null).map_err(ce),
        "read" => {
            let path = s(a, "path");
            if a.get("offset").is_some() {
                let off = py::to_int(&a["offset"]).unwrap_or(0).max(0) as u64;
                let len = (py::to_int(&a["length"]).unwrap_or(0).max(0) as u64).min(MAX_READ);
                return Ok(json!(util::b64(&h.read_range_local(&path, off, len).await.map_err(ce)?)));
            }
            let limit = py::to_int(&a["limit"]).filter(|l| *l != 0).unwrap_or(32 << 20) as usize;
            Ok(json!(util::b64(&h.read_local(&path, limit).await.map_err(ce)?)))
        }
        "write" => {
            let data = util::b64_decode(&s(a, "data")).ok_or(("Incorrect padding".to_string(), false))?;
            h.write_local(&s(a, "path"), &data).await.map(|_| Value::Null).map_err(ce)
        }
        "ls" => h.ls_local(&s(a, "path")).map(Value::from).map_err(|e| (e, true)),
        "stat" => {
            let paths: Vec<String> = a["paths"].as_array().map(|p| p.iter().map(py::str_of).collect()).unwrap_or_default();
            Ok(Value::Object(h.stat_local(&paths).into_iter().collect()))
        }
        "rm" => h.rm_local(&s(a, "path")).map(|_| Value::Null).map_err(|e| (e, true)),
        "exec" => {
            let secs = py::to_int(&a["secs"]).filter(|x| *x != 0).unwrap_or(120).clamp(1, 600);
            let stdin = if a["stdin"].is_null() { None } else { Some(py::str_of(&a["stdin"])) };
            let r = h.run(&s(a, "cmd"), secs, stdin.as_deref()).await.map_err(ce)?;
            Ok(json!({"exit_code": r.exit_code, "stdout": r.stdout, "stderr": r.stderr, "timed_out": r.timed_out}))
        }
        "desk_info" => {
            if macdesk::supported() {
                h.desk_info(false).await.map_err(ce)
            } else {
                Ok(json!({"supported": false}))
            }
        }
        "desk" | "capture" | "front" | "grant" => {
            if !macdesk::supported() {
                return Err(("这台电脑不是 Mac，不能操作整个屏幕".into(), false));
            }
            match op {
                "desk" => h.desk_local(&s(a, "action"), &a["a"]).await.map_err(ce),
                "capture" => {
                    let fmt = if py::truthy(&a["fmt"]) { s(a, "fmt") } else { "png".into() };
                    let q = py::to_int(&a["quality"]).filter(|q| *q != 0).unwrap_or(70);
                    Ok(json!(macdesk::capture(&fmt, q).await.map_err(|e| (e, false))?.0))
                }
                "front" => macdesk::front().await.map_err(|e| (e, false)),
                _ => macdesk::request_permissions().await.map(|_| Value::Null).map_err(|e| (e, false)),
            }
        }
        _ => Err((format!("不认识的操作：{op}"), false)),
    }
}

async fn reply(pc: Arc<Computer>, h: Arc<Host>, msg: Value, out: mpsc::UnboundedSender<Message>) {
    let mut r = json!({"id": msg["id"]});
    let op = py::str_of(&msg["op"]);
    let args = if msg["args"].is_object() { msg["args"].clone() } else { json!({}) };
    match handle(&pc, &h, &op, &args).await {
        Ok(v) => r["result"] = v,
        Err((e, value)) => {
            r["error"] = json!(if e.is_empty() { "Error".to_string() } else { e });
            if value {
                r["kind"] = json!("value");
            }
        }
    }
    let _ = out.send(Message::Text(py::dumps_u(&r).into()));
}

async fn hello(pc: &Arc<Computer>, device: &str) -> Value {
    let perms = if macdesk::supported() { macdesk::permissions().await.unwrap_or(json!({})) } else { json!({}) };
    json!({"type": "hello", "name": computer_name(), "platform": if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS },
           "desktop": macdesk::supported(), "perms": perms, "workspace": pc.workspace(), "shell": true, "ranges": true, "device": device})
}

/// 连上服务器；断了隔几秒重连，令牌作废了（4401）隔几分钟再试
pub async fn connect_loop(pc: Arc<Computer>, h: Arc<Host>, url: String, token: String) {
    let mut wait = 1u64;
    let device = tokio::task::spawn_blocking(device_id).await.unwrap_or_default();
    loop {
        let mut code: u16 = 0;
        let r: Result<(), String> = async {
            let mut req = url.as_str().into_client_request().map_err(|e| e.to_string())?;
            req.headers_mut().insert("authorization", format!("Bearer {token}").parse().map_err(|e: axum::http::header::InvalidHeaderValue| e.to_string())?);
            let cfg = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(MAX_MSG)).max_frame_size(Some(MAX_MSG));
            let (ws, _) = tokio::time::timeout(std::time::Duration::from_secs(20), tokio_tungstenite::connect_async_with_config(req, Some(cfg), false))
                .await
                .map_err(|_| "timeout".to_string())?
                .map_err(|e| e.to_string())?;
            let (mut sink, mut stream) = ws.split();
            sink.send(Message::Text(py::dumps_u(&hello(&pc, &device).await).into())).await.map_err(|e| e.to_string())?;
            tracing::info!("连上了 {url}");
            wait = 1;
            let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
            let writer = tokio::spawn(async move {
                let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
                ping.tick().await;
                loop {
                    tokio::select! {
                        m = rx.recv() => match m {
                            Some(m) => if sink.send(m).await.is_err() { break },
                            None => break,
                        },
                        _ = ping.tick() => if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break },
                    }
                }
            });
            while let Some(m) = stream.next().await {
                match m {
                    Ok(Message::Text(t)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            tokio::spawn(reply(pc.clone(), h.clone(), v, tx.clone()));
                        }
                    }
                    Ok(Message::Binary(b)) => {
                        if let Ok(v) = serde_json::from_slice::<Value>(&b) {
                            tokio::spawn(reply(pc.clone(), h.clone(), v, tx.clone()));
                        }
                    }
                    Ok(Message::Close(f)) => {
                        code = f.map(|f| u16::from(f.code)).unwrap_or(0);
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
            writer.abort();
            tracing::info!("连接断了（{code}）");
            Ok(())
        }
        .await;
        if let Err(e) = r {
            tracing::info!("连不上：{e}");
        }
        let delay = retry(code).unwrap_or(wait);
        wait = (wait * 2).min(30);
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
}

/// `osa-server agent`
pub async fn main() -> anyhow::Result<()> {
    let url = std::env::var("OSA_LINK_URL").unwrap_or_default();
    let token = std::env::var("OSA_LINK_TOKEN").unwrap_or_default();
    // SAFETY: 还没起别的线程用环境变量（tokio 的工作线程不读它）
    unsafe { std::env::remove_var("OSA_LINK_TOKEN") };
    if url.is_empty() || token.is_empty() {
        eprintln!("要 OSA_LINK_URL 和 OSA_LINK_TOKEN");
        std::process::exit(1);
    }
    let pc = Computer::new(0, Box::new(Host::new()), None);
    {
        let mut st = pc.st.lock();
        st.width = config::get_i64("computer.width", 1280);
        st.height = config::get_i64("computer.height", 800);
    }
    let parent: i32 = std::env::var("OSA_PARENT_PID").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
    let work = tokio::spawn(connect_loop(pc.clone(), Arc::new(Host::new()), url, token));
    let watch = async move {
        if parent == 0 {
            return std::future::pending::<()>().await;
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            #[cfg(unix)]
            {
                let alive = unsafe { libc::kill(parent, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                if !alive {
                    return;
                }
            }
        }
    };
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
            _ = watch => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = watch => {} }
    }
    tracing::info!("退出，关掉 Bot 的 Chrome");
    work.abort();
    if let Err(e) = tokio::time::timeout(std::time::Duration::from_secs(10), pc.pause()).await.unwrap_or(Ok(())) {
        tracing::info!("关 Chrome 没成功：{e}");
    }
    Ok(())
}

#[allow(dead_code)]
fn _p(_: Place) -> R<()> {
    Ok(())
}
