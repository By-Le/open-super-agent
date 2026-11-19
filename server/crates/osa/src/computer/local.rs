//! Docker 沙盒：所有 Bot 共用一个电脑容器。对应 osa/computer/local.py（两种用法见那里的说明）：
//! 服务直接跑在机器上时用 docker 命令自己管容器 osa-computer；Docker Compose 部署（设了 OSA_COMPUTER_SOCKET）时
//! 经共用卷里的 unix socket 找电脑里的 computerd。

use super::base::{Backend, Computer, ComputerError, ExecResult, R, Vnc, check_enabled, err};
use crate::{config};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

pub const IMAGE: &str = "osa-computer:2";

pub fn socket() -> String {
    std::env::var("OSA_COMPUTER_SOCKET").unwrap_or_default()
}

pub fn image_dir() -> std::path::PathBuf {
    config::ROOT.join("osa").join("computer").join("image")
}

const VNC_RELAY: &str = r#"
import os, socket, sys, threading, time
for _ in range(50):
    try:
        s = socket.create_connection(("127.0.0.1", 5900))
        break
    except OSError:
        time.sleep(0.2)
else:
    sys.exit("VNC 没有启动")
def up():
    while True:
        d = os.read(0, 65536)
        if not d:
            break
        s.sendall(d)
    s.shutdown(socket.SHUT_WR)
threading.Thread(target=up, daemon=True).start()
out = sys.stdout.buffer
while True:
    d = s.recv(65536)
    if not d:
        break
    out.write(d)
    out.flush()
"#;

/// 跑一条 docker 命令：(退出码, stdout, stderr)
pub async fn run(args: &[&str], stdin: Option<&str>, timeout: f64) -> (i64, String, String) {
    let mut cmd = tokio::process::Command::new(args[0]);
    cmd.args(&args[1..]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    cmd.stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() });
    let mut p = match cmd.spawn() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (127, String::new(), t!("这台电脑没装 Docker")),
        Err(e) => return (127, String::new(), e.to_string()),
    };
    if let (Some(s), Some(mut w)) = (stdin, p.stdin.take()) {
        let data = s.as_bytes().to_vec();
        tokio::spawn(async move {
            let _ = w.write_all(&data).await;
            let _ = w.shutdown().await;
        });
    }
    match tokio::time::timeout(Duration::from_secs_f64(timeout), p.wait_with_output()).await {
        Ok(Ok(o)) => (
            o.status.code().unwrap_or(-1) as i64,
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Ok(Err(e)) => (1, String::new(), e.to_string()),
        Err(_) => (124, String::new(), t!("超时")),
    }
}

static PLATFORM: Mutex<Option<String>> = Mutex::new(None);

fn problem(e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("cannot connect to the docker daemon") || l.contains("docker daemon is not running") || l.contains("docker.sock") {
        return t!("Docker 没在运行：先打开 Docker Desktop（或 OrbStack），再点启动");
    }
    crate::py::tail(e.trim(), 500)
}

/// 用 Docker 引擎的原生架构
pub async fn platform() -> String {
    if let Some(p) = PLATFORM.lock().clone() {
        return p;
    }
    let forced = config::get_str("computer.local_platform");
    let p = if !forced.is_empty() {
        forced
    } else {
        let (_, out, _) = run(&["docker", "info", "--format", "{{.Architecture}}"], None, 20.0).await;
        let arch = match out.trim() {
            "aarch64" | "arm64" => "arm64",
            "x86_64" => "amd64",
            _ => "amd64",
        };
        format!("linux/{arch}")
    };
    *PLATFORM.lock() = Some(p.clone());
    p
}

/// 经 unix socket 让电脑里的 computerd 做一件事（一次连接一件），返回它回的那行 JSON
#[cfg(unix)]
async fn ask(req: &Value, timeout: f64) -> R<Value> {
    let conn = tokio::time::timeout(Duration::from_secs(10), tokio::net::UnixStream::connect(socket())).await;
    let Ok(Ok(stream)) = conn else {
        return err(t!("电脑容器没在运行（docker compose up -d computer）"));
    };
    let (r, mut w) = stream.into_split();
    let mut line = crate::py::dumps_u(req);
    line.push('\n');
    let mut reader = tokio::io::BufReader::with_capacity(1 << 20, r);
    let res = tokio::time::timeout(Duration::from_secs_f64(timeout), async {
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        let mut out = String::new();
        reader.read_line(&mut out).await?;
        Ok::<String, std::io::Error>(out)
    })
    .await;
    let out = match res {
        Err(_) => return err(t!("超时")),
        Ok(Err(e)) => return err(e.to_string()),
        Ok(Ok(o)) => o,
    };
    if out.is_empty() {
        return err(t!("电脑容器没有回应"));
    }
    let v: Value = serde_json::from_str(&out).map_err(|e| ComputerError(e.to_string()))?;
    if let Some(e) = v.get("error") {
        return err(crate::py::str_of(e));
    }
    Ok(v)
}

#[cfg(not(unix))]
async fn ask(_req: &Value, _timeout: f64) -> R<Value> {
    err(t!("电脑容器没在运行（docker compose up -d computer）"))
}

pub struct Local {
    pub name: String,
    pub data: String,
    starting: tokio::sync::Mutex<()>,
    building: Mutex<bool>,
    idle: Mutex<bool>,
}

impl Local {
    pub fn new() -> Local {
        let data = std::env::var("OSA_COMPUTER_DATA")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| config::DATA.join("computers").join("shared").display().to_string());
        Local { name: "osa-computer".into(), data, starting: tokio::sync::Mutex::new(()), building: Mutex::new(false), idle: Mutex::new(false) }
    }

    async fn ensure_image(&self, c: &Computer) -> R<()> {
        let (code, _, e) = run(&["docker", "image", "inspect", IMAGE], None, 15.0).await;
        if code != 0 && (code == 127 || !e.to_lowercase().contains("no such")) {
            return err(problem(&e));
        }
        if code != 0 {
            *self.building.lock() = true;
            c.changed();
            let plat = platform().await;
            let dir = image_dir().display().to_string();
            let (code, out, e) = run(&["docker", "build", "--platform", &plat, "-t", IMAGE, &dir], None, 1800.0).await;
            *self.building.lock() = false;
            c.changed();
            if code != 0 {
                return err(t!("电脑镜像构建失败：{error}", error = crate::py::tail(if e.is_empty() { &out } else { &e }, 800)));
            }
        }
        Ok(())
    }

    async fn outdated(&self) -> bool {
        let (code, out, _) = run(&["docker", "inspect", "-f", "{{.Config.Image}}", &self.name], None, 15.0).await;
        code == 0 && !["", IMAGE].contains(&out.trim())
    }
}

#[async_trait]
impl Backend for Local {
    fn kind(&self) -> &'static str {
        "local"
    }

    async fn status(&self, _c: &Computer) -> Value {
        if !config::get_bool("computer.enabled", true) {
            return json!({"state": "none", "detail": ""});
        }
        if !socket().is_empty() {
            return match ask(&json!({"op": "ready"}), 15.0).await {
                Err(e) => json!({"state": "error", "detail": e.0}),
                Ok(v) => {
                    let ok = v["ok"] == true;
                    json!({"state": if !ok { "starting" } else if *self.idle.lock() { "paused" } else { "running" }, "detail": ""})
                }
            };
        }
        if *self.building.lock() {
            return json!({"state": "starting", "detail": t!("第一次用要先准备电脑（下载、安装浏览器和桌面），要几分钟")});
        }
        let (code, out, e) = run(&["docker", "inspect", "-f", "{{json .State}}", &self.name], None, 15.0).await;
        if code != 0 {
            if code == 127 || !e.to_lowercase().contains("no such") {
                return json!({"state": "error", "detail": problem(&e)});
            }
            return json!({"state": "none", "detail": t!("还没有创建")});
        }
        let st: Value = serde_json::from_str(out.trim()).unwrap_or(json!({}));
        let status = st["Status"].as_str().unwrap_or("");
        let state = match status {
            "running" => "running",
            "paused" => "paused",
            "exited" | "created" => "stopped",
            "restarting" => "starting",
            _ => "error",
        };
        json!({"state": state, "detail": status})
    }

    async fn start(&self, c: &Computer) -> R<()> {
        check_enabled()?;
        if !socket().is_empty() {
            *self.idle.lock() = false;
            for _ in 0..60 {
                if self.status(c).await["state"] == "running" {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            return err(t!("桌面没有在 30 秒内启动"));
        }
        let _l = self.starting.lock().await;
        let mut st = self.status(c).await["state"].as_str().unwrap_or("").to_string();
        if st == "running" {
            return Ok(());
        }
        if (st == "paused" || st == "stopped") && self.outdated().await {
            let (code, out, e) = run(&["docker", "rm", "-f", &self.name], None, 60.0).await;
            if code != 0 {
                return err(problem(if e.is_empty() { &out } else { &e }));
            }
            st = "none".into();
        }
        if st == "paused" {
            run(&["docker", "unpause", &self.name], None, 120.0).await;
        } else if st == "stopped" {
            run(&["docker", "start", &self.name], None, 120.0).await;
        } else {
            self.ensure_image(c).await?;
            if std::path::Path::new(&self.data).is_absolute() {
                let _ = std::fs::create_dir_all(&self.data);
            }
            let (w, h) = {
                let s = c.st.lock();
                (s.width, s.height)
            };
            let plat = platform().await;
            let vol = format!("{}:/data", self.data);
            let sw = format!("SCREEN_W={w}");
            let sh = format!("SCREEN_H={h}");
            let (code, out, e) = run(
                &[
                    "docker", "run", "-d", "--platform", &plat, "--name", &self.name, "--hostname", "osa", "--shm-size=1g", "-v", &vol, "-e", &sw, "-e", &sh,
                    "--label", "osa=computer", IMAGE,
                ],
                None,
                120.0,
            )
            .await;
            if code != 0 {
                return err(t!("电脑启动失败：{error}", error = crate::py::tail(if e.is_empty() { &out } else { &e }, 500)));
            }
        }
        for _ in 0..60 {
            if self.exec(c, "DISPLAY=:1 xdotool getdisplaygeometry", 10, None).await?.exit_code == 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        err(t!("桌面没有在 30 秒内启动"))
    }

    async fn pause(&self, c: &Computer) -> R<()> {
        if !socket().is_empty() {
            if self.status(c).await["state"] == "running" {
                ask(&json!({"op": "exec", "cmd": "pkill -TERM -f /usr/lib/chromium || true", "timeout": 20}), 30.0).await?;
                *self.idle.lock() = true;
                c.changed();
            }
            return Ok(());
        }
        if self.status(c).await["state"] == "running" {
            run(&["docker", "pause", &self.name], None, 120.0).await;
        }
        Ok(())
    }

    async fn exec(&self, _c: &Computer, cmd: &str, timeout: i64, stdin: Option<&str>) -> R<ExecResult> {
        if !socket().is_empty() {
            *self.idle.lock() = false;
            return Ok(match ask(&json!({"op": "exec", "cmd": cmd, "stdin": stdin, "timeout": timeout}), (timeout + 15) as f64).await {
                Err(e) => ExecResult::new(1, "", e.0, false),
                Ok(r) => ExecResult::new(
                    crate::py::to_int(&r["exit_code"]).unwrap_or(1),
                    r["stdout"].as_str().unwrap_or(""),
                    r["stderr"].as_str().unwrap_or(""),
                    crate::py::truthy(&r["timed_out"]),
                ),
            });
        }
        let (code, out, e) = run(&["docker", "exec", "-i", "-w", "/data/workspace", &self.name, "bash", "-lc", cmd], Some(stdin.unwrap_or("")), (timeout + 5) as f64).await;
        Ok(ExecResult::new(code, out, e, code == 124))
    }

    async fn open_vnc(&self, _c: &Computer) -> R<Vnc> {
        if !socket().is_empty() {
            return socket_vnc().await;
        }
        let mut p = tokio::process::Command::new("docker")
            .args(["exec", "-i", &self.name, "python3", "-c", VNC_RELAY])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ComputerError(e.to_string()))?;
        let rx = p.stdout.take().ok_or_else(|| ComputerError("no stdout".into()))?;
        let tx = p.stdin.take().ok_or_else(|| ComputerError("no stdin".into()))?;
        Ok(Vnc::Io { rx: Box::new(rx), tx: Box::new(tx), child: Some(p) })
    }
}

#[cfg(unix)]
async fn socket_vnc() -> R<Vnc> {
    let conn = tokio::time::timeout(Duration::from_secs(10), tokio::net::UnixStream::connect(socket())).await;
    let Ok(Ok(mut stream)) = conn else {
        return err(t!("电脑容器没在运行（docker compose up -d computer）"));
    };
    stream.write_all(b"{\"op\": \"vnc\"}\n").await.map_err(|e| ComputerError(e.to_string()))?;
    // 读回第一行（不能用 BufReader：后面的 VNC 字节不能被它吃掉）
    let mut line = vec![];
    let mut b = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match stream.read(&mut b).await {
                Ok(0) => break,
                Ok(_) => {
                    if b[0] == b'\n' {
                        break;
                    }
                    line.push(b[0]);
                }
                Err(_) => break,
            }
        }
    })
    .await;
    let ack: Value = if read.is_ok() { serde_json::from_slice(&line).unwrap_or(json!({})) } else { json!({}) };
    if ack["ok"] != true {
        return err(ack["error"].as_str().map(String::from).unwrap_or_else(|| t!("电脑容器没有回应")));
    }
    let (r, w) = stream.into_split();
    Ok(Vnc::Io { rx: Box::new(r), tx: Box::new(w), child: None })
}

#[cfg(not(unix))]
async fn socket_vnc() -> R<Vnc> {
    err(t!("电脑容器没在运行（docker compose up -d computer）"))
}

/// unix socket 那头有没有在听
pub fn listening(path: &str) -> bool {
    #[cfg(unix)]
    {
        let s = std::os::unix::net::UnixStream::connect(path);
        s.is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[allow(dead_code)]
static _UNUSED: LazyLock<()> = LazyLock::new(|| ());
