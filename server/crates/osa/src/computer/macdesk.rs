//! macOS 的整个屏幕、真实的鼠标键盘（对应 osa/computer/macdesk.py）。
//!
//! macdesk.py 原样留着（ctypes 调 CoreGraphics / 辅助功能接口），由一个常驻的 Python 小进程跑：Rust 这边按行发
//! {"fn": 函数名, "args": [...]}，它调 macdesk 里的同名函数，按行回 {"ok": 结果} 或 {"error": 原因}（bytes 用 {"__b64__": …}）。
//! Python 按 sandbox.rs 的顺序找（OSA_PYTHON / python3 / uv python，第一次用到时才下载）。权限照旧记在启动服务的 App 名下
//! （小进程是服务的子进程，macOS 按“负责的进程”算）。不是 macOS 时 supported() 为 false，别的函数不要调。

use crate::config;
use serde_json::{Value, json};
use std::sync::LazyLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

static SOURCE: &str = include_str!("../../../../../osa/computer/macdesk.py");

const SERVE: &str = r#"import base64, json, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import macdesk as m
ALLOWED = {"supported", "permissions", "request_permissions", "locked", "idle", "screen", "to_screen", "from_screen",
           "cursor", "capture", "click", "drag", "scroll", "type_text", "press", "press_seq", "front", "windows",
           "element_at", "focused", "open_app", "find_app"}
def enc(v):
    if isinstance(v, (bytes, bytearray)):
        return {"__b64__": base64.b64encode(bytes(v)).decode()}
    if isinstance(v, (list, tuple)):
        return [enc(x) for x in v]
    if isinstance(v, dict):
        return {k: enc(x) for k, x in v.items()}
    return v
for line in sys.stdin:
    try:
        req = json.loads(line)
        fn = str(req.get("fn") or "")
        if fn not in ALLOWED:
            raise ValueError("unknown " + fn)
        out = {"ok": enc(getattr(m, fn)(*(req.get("args") or [])))}
    except Exception as e:
        out = {"error": str(e) or type(e).__name__}
    sys.stdout.write(json.dumps(out, ensure_ascii=False) + "\n")
    sys.stdout.flush()
"#;

struct Proc {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

static PROC: LazyLock<Mutex<Option<Proc>>> = LazyLock::new(|| Mutex::new(None));

pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

fn dir() -> std::path::PathBuf {
    use sha1::Digest;
    let h: String = sha1::Sha1::digest(format!("{SOURCE}{SERVE}").as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
    config::DATA.join("computers").join("host").join(format!("macdesk-{h}"))
}

async fn spawn() -> Result<Proc, String> {
    let py = crate::sandbox::python().await.ok_or_else(|| "找不到 Python：操作整个屏幕要用它（装一个 Python 3，或者设环境变量 OSA_PYTHON）".to_string())?;
    let d = dir();
    if !d.join("serve.py").is_file() {
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        std::fs::write(d.join("macdesk.py"), SOURCE).map_err(|e| e.to_string())?;
        std::fs::write(d.join("serve.py"), SERVE).map_err(|e| e.to_string())?;
    }
    let mut child = tokio::process::Command::new(py)
        .arg("-I")
        .arg(d.join("serve.py"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    Ok(Proc { _child: child, stdin, stdout })
}

fn dec(v: Value) -> Value {
    match v {
        Value::Object(o) if o.len() == 1 && o.contains_key("__b64__") => Value::String(o["__b64__"].as_str().unwrap_or("").to_string()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, dec(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().map(dec).collect()),
        v => v,
    }
}

/// 调 macdesk 里的一个函数。bytes 的结果是 base64 文字
pub async fn call(func: &str, args: Value) -> Result<Value, String> {
    let mut g = PROC.lock().await;
    for attempt in 0..2 {
        if g.is_none() {
            *g = Some(spawn().await?);
        }
        let p = g.as_mut().unwrap();
        let mut line = serde_json::to_string(&json!({"fn": func, "args": args})).unwrap_or_default();
        line.push('\n');
        let r: Result<String, String> = async {
            p.stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
            p.stdin.flush().await.map_err(|e| e.to_string())?;
            let mut out = String::new();
            let n = tokio::time::timeout(std::time::Duration::from_secs(60), p.stdout.read_line(&mut out))
                .await
                .map_err(|_| "超时".to_string())?
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("closed".into());
            }
            Ok(out)
        }
        .await;
        match r {
            Ok(out) => {
                let v: Value = serde_json::from_str(out.trim()).map_err(|e| e.to_string())?;
                if let Some(e) = v.get("error") {
                    return Err(crate::py::str_of(e));
                }
                return Ok(dec(v.get("ok").cloned().unwrap_or(Value::Null)));
            }
            Err(e) => {
                *g = None;
                if attempt == 1 {
                    return Err(e);
                }
            }
        }
    }
    Err("macdesk".into())
}

pub async fn permissions() -> Result<Value, String> {
    call("permissions", json!([])).await
}

pub async fn request_permissions() -> Result<Value, String> {
    call("request_permissions", json!([])).await
}

pub async fn locked() -> Result<bool, String> {
    Ok(crate::py::truthy(&call("locked", json!([])).await?))
}

pub async fn idle() -> Result<f64, String> {
    Ok(call("idle", json!([])).await?.as_f64().unwrap_or(0.0))
}

fn pair(v: &Value) -> (f64, f64) {
    (v[0].as_f64().unwrap_or(0.0), v[1].as_f64().unwrap_or(0.0))
}

pub async fn to_screen(x: f64, y: f64) -> Result<(f64, f64), String> {
    Ok(pair(&call("to_screen", json!([x, y])).await?))
}

pub async fn from_screen(x: f64, y: f64) -> Result<(i64, i64), String> {
    let (a, b) = pair(&call("from_screen", json!([x, y])).await?);
    Ok((a as i64, b as i64))
}

pub async fn cursor() -> Result<(f64, f64), String> {
    Ok(pair(&call("cursor", json!([])).await?))
}

/// (png 的 base64, 宽, 高)
pub async fn capture(fmt: &str, quality: i64) -> Result<(String, i64, i64), String> {
    let v = call("capture", json!([fmt, quality])).await?;
    Ok((v[0].as_str().unwrap_or("").to_string(), v[1].as_i64().unwrap_or(0), v[2].as_i64().unwrap_or(0)))
}

pub async fn front() -> Result<Value, String> {
    call("front", json!([])).await
}

pub async fn windows(limit: i64) -> Result<Vec<Value>, String> {
    Ok(call("windows", json!([limit])).await?.as_array().cloned().unwrap_or_default())
}

pub async fn element_at(x: f64, y: f64) -> Result<Value, String> {
    call("element_at", json!([x, y])).await
}

pub async fn focused() -> Result<Value, String> {
    call("focused", json!([])).await
}
