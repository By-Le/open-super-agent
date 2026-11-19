//! 本机电脑（osa/computer/host.py）和派到你的 Mac 上的任务用的电脑（osa/computer/mac.py）。说明见那两个文件。
//!
//! Python 版里 MacComputer 继承 HostComputer：排队、确认、你一动鼠标就停这些在服务器上照常做，真正动手的那一步换成经连接交给 Mac。
//! 这里是同一个类型 `Host`，`place` 决定动手的那一步在哪做：`Local`（这台机器：本机跑 cdp_helper.py、读写工作文件夹、macdesk）
//! 或 `Mac(编号)`（经 link.rs 的连接让那台 Mac 上的小程序做，见 agent.rs）。

use super::base::{Backend, Computer, ComputerError, ExecResult, R, SHELL_MAX_S, Vnc, check_enabled, err};
use super::{link, macdesk};
use crate::{approvals, config, py, security, util};
use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub use super::pc::chrome_path;

pub const LEGACY: &str = "/data/workspace";
pub const DESK_CHAT: f64 = 120.0;
pub const CHAT_OK: f64 = 600.0;
pub const QUIET: f64 = 0.5;
pub const DESK_STALE: &str = "用户刚才在用电脑（或者接手过），屏幕可能已经变了，这一步没有执行。先 desktop_screenshot 看一下屏幕再继续";
pub const OWN_TITLE: &str = "Open Super Agent";
/// 旧版 App 的小程序不会按段读，下载、播放只能整个读，最大这么多
pub const OLD_MAX: usize = 40 << 20;

static SECRET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)KEY|TOKEN|SECRET|PASSW|CREDENTIAL|^OSA_").unwrap());

/// 给控制脚本、Chrome 和终端命令的环境变量：去掉密钥和服务自己的设置
pub fn clean_env() -> Vec<(String, String)> {
    std::env::vars().filter(|(k, _)| !SECRET.is_match(k)).collect()
}

pub fn shell_on() -> bool {
    config::get_bool("computer.host_shell", false) && security::local_exec()
}

fn host_desktop() -> bool {
    config::get_bool("computer.host_desktop", true)
}

/// 是个文件：Some(true)；不存在或不是文件：Some(false)；有但没权限看：None
fn probe(path: &str) -> Option<bool> {
    match std::fs::metadata(path) {
        Ok(m) => Some(m.is_file()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => None,
        Err(_) => Some(false),
    }
}

/// Open Super Agent 自己的进程：服务自己、桌面 App（服务的父进程）
fn own_pids() -> Vec<i64> {
    let mut out = vec![std::process::id() as i64];
    if let Ok(p) = std::env::var("OSA_PARENT_PID") {
        out.push(p.parse::<i64>().unwrap_or(0));
    }
    out.retain(|p| *p != 0);
    out
}

pub enum Place {
    Local,
    Mac(i64),
}

#[derive(Default)]
struct Desk {
    owner: String,
    label: String,
    seen: f64,
    at: f64,
    ok: HashMap<String, f64>,
    denied: HashMap<String, f64>,
    paused_by: String,
    info: (f64, Value),
    whole: (String, f64, Vec<u8>),
}

pub struct Host {
    pub place: Place,
    root: PathBuf,
    profile: PathBuf,
    desk: Mutex<Desk>,
    desk_free: tokio::sync::Notify,
    starting: tokio::sync::Mutex<()>,
    whole_lock: tokio::sync::Mutex<()>,
}

impl Host {
    /// 这台机器
    pub fn new() -> Host {
        let root = config::DATA.join("workspace");
        let _ = std::fs::create_dir_all(&root);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Host::make(Place::Local, root, config::DATA.join("computers").join("host").join("chrome"))
    }

    /// 派到的那台 Mac（不在服务器上建工作文件夹和 Chrome 配置目录）
    pub fn mac(lid: i64) -> Host {
        Host::make(Place::Mac(lid), PathBuf::new(), PathBuf::new())
    }

    fn make(place: Place, root: PathBuf, profile: PathBuf) -> Host {
        Host {
            place,
            root,
            profile,
            desk: Mutex::new(Desk::default()),
            desk_free: tokio::sync::Notify::new(),
            starting: tokio::sync::Mutex::new(()),
            whole_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn lid_(&self) -> i64 {
        match self.place {
            Place::Mac(l) => l,
            Place::Local => 0,
        }
    }

    fn mac_name(&self) -> String {
        link::row(self.lid_()).map(|r| py::str_of(&r["name"])).unwrap_or_else(|| "Mac".into())
    }

    fn link(&self) -> R<std::sync::Arc<link::Link>> {
        link::connected(self.lid_()).ok_or_else(|| ComputerError(link::offline(&self.mac_name())))
    }

    async fn call(&self, op: &str, args: Value) -> R<Value> {
        self.call_t(op, link::TIMEOUT, args).await
    }

    async fn call_t(&self, op: &str, timeout: f64, args: Value) -> R<Value> {
        self.link()?.call(op, timeout, args).await
    }

    // ---------- 浏览器 ----------
    fn helper_file(&self) -> PathBuf {
        let name = super::base::HELPER_PATH.rsplit('/').next().unwrap_or("osa-cdp.py").to_string();
        config::DATA.join("computers").join("host").join(name)
    }

    /// 本机跑一次浏览器控制脚本
    pub async fn helper_local(&self, c: &Computer, payload: &Value) -> R<Value> {
        let f = self.helper_file();
        if !f.is_file() {
            if let Some(d) = f.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            std::fs::write(&f, super::base::HELPER).map_err(|e| ComputerError(e.to_string()))?;
        }
        let py_exe = crate::sandbox::python()
            .await
            .ok_or_else(|| ComputerError("找不到 Python：Bot 的 Chrome 由一个 Python 小脚本控制（装一个 Python 3，或者设环境变量 OSA_PYTHON）".into()))?;
        let (w, h) = {
            let s = c.st.lock();
            (s.width, s.height)
        };
        let mut cmd = tokio::process::Command::new(py_exe);
        cmd.arg("-I").arg(&f).env_clear().envs(clean_env());
        cmd.env("OSA_CDP_PROFILE", &self.profile)
            .env("OSA_CDP_CHROME", chrome_path().unwrap_or_default())
            .env("OSA_CDP_SIZE", format!("{w},{}", h + 90))
            .env("OSA_CDP_DOWNLOADS", self.root.join("downloads"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut p = cmd.spawn().map_err(|e| ComputerError(e.to_string()))?;
        let input = py::dumps_u(payload);
        let mut stdin = p.stdin.take();
        let fut = async {
            if let Some(mut s) = stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = s.write_all(input.as_bytes()).await;
                drop(s);
            }
            p.wait_with_output().await
        };
        let out = match tokio::time::timeout(std::time::Duration::from_secs(90), fut).await {
            Err(_) => return err(t!("超时")),
            Ok(Err(e)) => return err(e.to_string()),
            Ok(Ok(o)) => o,
        };
        let so = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            let last = so.trim().lines().last().unwrap_or("");
            return serde_json::from_str(last).map_err(|e| ComputerError(e.to_string()));
        }
        let se = String::from_utf8_lossy(&out.stderr).into_owned();
        let e = if se.trim().is_empty() { so.trim().to_string() } else { se.trim().to_string() };
        let e = if e.is_empty() { t!("退出码 {code}", code = out.status.code().unwrap_or(-1)) } else { e };
        let last = e.lines().last().unwrap_or("");
        err(py::head(last.strip_prefix("RuntimeError: ").unwrap_or(last), 500))
    }

    fn port(&self) -> i64 {
        std::fs::read_to_string(self.profile.join("DevToolsActivePort"))
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|p| p.parse().ok()))
            .unwrap_or(0)
    }

    async fn status_local(&self) -> Value {
        let Some(path) = chrome_path() else {
            return json!({"state": "error", "detail": t!("没找到 Chrome：先安装 Google Chrome，或者在 config.yaml 的 computer.chrome_path 写上它的路径")});
        };
        if probe(&path).is_none() {
            return json!({"state": "error", "detail": t!("没权限打开 Chrome（{path}），它可能在外接硬盘上。到 系统设置 → 隐私与安全性 → 文件与文件夹，给 Open Super Agent 打开“可移除的宗卷”，再重新打开 App", path = path)});
        }
        let port = self.port();
        if port != 0 {
            let ok = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(2))
                .build()
                .ok()
                .map(|h| h.get(format!("http://127.0.0.1:{port}/json/version")).send());
            if let Some(f) = ok {
                if let Ok(r) = f.await {
                    if r.status().as_u16() == 200 {
                        return json!({"state": "running", "detail": ""});
                    }
                }
            }
        }
        json!({"state": "stopped", "detail": ""})
    }

    fn view(c: &Computer) -> (Value, Value) {
        let s = c.st.lock();
        if let Some(w) = &s.watch {
            if let Some(u) = s.users.get(w).filter(|u| py::truthy(&u.window)) {
                return (u.window.clone(), u.browser.clone());
            }
        }
        s.recent.clone()
    }

    async fn peek_page(&self, c: &Computer, fmt: &str) -> R<Value> {
        if c.state().await != "running" {
            return err(t!("Bot 用的 Chrome 没开"));
        }
        let (win, browser) = Host::view(c);
        c.helper_run(&json!({"op": "peek", "window": win, "browser": browser, "format": fmt})).await
    }

    async fn show_screen(&self, c: &Computer, view: &str) -> bool {
        if view == "chrome" || !self.desktop_enabled() {
            return false;
        }
        let Ok(info) = self.desk_info(false).await else { return false };
        if !self.desk_problem(&info, true).is_empty() {
            return false;
        }
        view == "screen" || c.st.lock().recent_desk || self.desk_busy() || c.state().await != "running"
    }

    // ---------- 整台电脑 ----------
    /// 那台电脑现在的情况：两个权限、锁没锁屏、多久没有键盘鼠标输入
    pub async fn desk_info(&self, fresh: bool) -> R<Value> {
        match self.place {
            Place::Local => {
                let mut info = json!({"supported": macdesk::supported()});
                let p = macdesk::permissions().await.map_err(ComputerError)?;
                perm_cache::remember(&p);
                for k in ["accessibility", "screen"] {
                    info[k] = p[k].clone();
                }
                info["locked"] = json!(macdesk::locked().await.map_err(ComputerError)?);
                info["idle"] = json!(macdesk::idle().await.map_err(ComputerError)?);
                Ok(info)
            }
            Place::Mac(_) => {
                let (at, info) = self.desk.lock().info.clone();
                if fresh || py::now() - at > 1.0 || info.is_null() {
                    let info = self.call("desk_info", json!({})).await?;
                    self.desk.lock().info = (py::now(), info.clone());
                    return Ok(info);
                }
                Ok(info)
            }
        }
    }

    pub fn desk_problem(&self, info: &Value, look: bool) -> String {
        if !self.desk_supported() || !host_desktop() {
            return t!("这台电脑不能让 Bot 操作整个屏幕（只有 Mac 可以）");
        }
        let missing: Vec<String> = [("accessibility", "辅助功能"), ("screen", "屏幕录制")]
            .iter()
            .filter(|(k, _)| !py::truthy(&info[*k]) && !(look && *k == "accessibility"))
            .map(|(_, n)| t!(n))
            .collect();
        if !missing.is_empty() {
            return t!("还没给 Open Super Agent 这些权限：{what}。电脑面板里点“去打开”，或者到 系统设置 → 隐私与安全性 里打开", what = missing.join(&t!("、")));
        }
        if !look && py::truthy(&info["locked"]) {
            return t!("屏幕锁着（或者显示器睡着了），现在不能操作");
        }
        String::new()
    }

    fn held(&self, key: &str) -> bool {
        let d = self.desk.lock();
        let o = &d.owner;
        !o.is_empty() && o != key && !(o.starts_with("chat-") && py::now() - d.seen > DESK_CHAT)
    }

    fn user_active(&self, info: &Value) -> bool {
        let at = self.desk.lock().at;
        if at == 0.0 {
            return false;
        }
        let since = at.max(*approvals::LAST_DONE.lock());
        (py::now() - since) - info["idle"].as_f64().unwrap_or(0.0) > QUIET
    }

    async fn capture(&self, fmt: &str, quality: i64) -> R<String> {
        match self.place {
            Place::Local => Ok(macdesk::capture(fmt, quality).await.map_err(ComputerError)?.0),
            Place::Mac(_) => Ok(py::str_of(&self.call("capture", json!({"fmt": fmt, "quality": quality})).await?)),
        }
    }

    async fn front_app(&self) -> R<Value> {
        match self.place {
            Place::Local => macdesk::front().await.map_err(ComputerError),
            Place::Mac(_) => self.call("front", json!({})).await,
        }
    }

    /// 不碰 Open Super Agent 自己的窗口
    async fn guard(&self, op: &str, a: &Value) -> R<()> {
        let own = own_pids();
        let hit = match op {
            "click" | "drag" | "scroll" | "inspect" => {
                let wins = macdesk::windows(12).await.map_err(ComputerError)?;
                let mut pts = vec![(a["x"].as_f64().unwrap_or(0.0), a["y"].as_f64().unwrap_or(0.0))];
                if op == "drag" {
                    pts.push((a["to_x"].as_f64().unwrap_or(0.0), a["to_y"].as_f64().unwrap_or(0.0)));
                }
                let mut hit = false;
                for (x, y) in pts {
                    let f = |w: &Value, k: &str| w[k].as_f64().unwrap_or(0.0);
                    let win = wins.iter().find(|w| f(w, "x") <= x && x < f(w, "x") + f(w, "w") && f(w, "y") <= y && y < f(w, "y") + f(w, "h")).cloned().unwrap_or(json!({}));
                    let el = macdesk::element_at(x, y).await.map_err(ComputerError)?;
                    let el_pid = el["pid"].as_i64().unwrap_or(0);
                    let win_pid = win["pid"].as_i64();
                    hit = hit || own.contains(&el_pid) || win_pid.is_some_and(|p| own.contains(&p)) || win["title"].as_str().unwrap_or("").contains(OWN_TITLE);
                }
                hit
            }
            "type" | "key" => {
                let f = macdesk::front().await.map_err(ComputerError)?;
                own.contains(&f["pid"].as_i64().unwrap_or(0)) || f["title"].as_str().unwrap_or("").contains(OWN_TITLE)
            }
            _ => return Ok(()),
        };
        if hit {
            return err("那里是 Open Super Agent 自己的窗口，Bot 不能操作它（确认卡片、设置只能用户自己点）。要操作别的 App，先用 desktop_open_app 把它切到前面。");
        }
        Ok(())
    }

    /// 本机做一个屏幕动作（host.py 的 _desk_sync）。截图放在 "png"（base64）
    pub async fn desk_local(&self, op: &str, a: &Value) -> R<Value> {
        let mut a = if a.is_object() { a.clone() } else { json!({}) };
        let num = |v: &Value| v.as_f64().or_else(|| py::to_int(v).map(|i| i as f64)).ok_or_else(|| ComputerError(format!("'{}'", "x")));
        if matches!(op, "click" | "drag" | "scroll" | "inspect") {
            let (x, y) = macdesk::to_screen(num(&a["x"])?, num(&a["y"])?).await.map_err(ComputerError)?;
            a["x"] = json!(x);
            a["y"] = json!(y);
        }
        if op == "drag" {
            let (x, y) = macdesk::to_screen(num(&a["to_x"])?, num(&a["to_y"])?).await.map_err(ComputerError)?;
            a["to_x"] = json!(x);
            a["to_y"] = json!(y);
        }
        self.guard(op, &a).await?;
        let m = |e: String| ComputerError(e);
        match op {
            "screenshot" => {
                let (png, w, h) = macdesk::capture("png", 70).await.map_err(m)?;
                let (cx, cy) = macdesk::cursor().await.map_err(m)?;
                let (cx, cy) = macdesk::from_screen(cx, cy).await.map_err(m)?;
                return Ok(json!({"png": png, "width": w, "height": h, "front": macdesk::front().await.map_err(m)?,
                                 "windows": macdesk::windows(8).await.map_err(m)?, "cursor": [cx, cy]}));
            }
            "inspect" => {
                let mut el = macdesk::element_at(a["x"].as_f64().unwrap_or(0.0), a["y"].as_f64().unwrap_or(0.0)).await.map_err(m)?;
                let wins = macdesk::windows(12).await.map_err(m)?;
                let app = wins.iter().find(|w| w["pid"] == el["pid"]).map(|w| w["app"].clone()).unwrap_or(json!(""));
                el["app"] = app;
                return Ok(el);
            }
            "focused" => {
                let mut f = macdesk::focused().await.map_err(m)?;
                f["app"] = macdesk::front().await.map_err(m)?["app"].clone();
                return Ok(f);
            }
            "click" => {
                let button = if py::truthy(&a["button"]) { py::str_of(&a["button"]) } else { "left".into() };
                macdesk::call("click", json!([a["x"], a["y"], button, if py::truthy(&a["double"]) { 2 } else { 1 }])).await.map_err(m)?;
            }
            "drag" => {
                macdesk::call("drag", json!([a["x"], a["y"], a["to_x"], a["to_y"]])).await.map_err(m)?;
            }
            "scroll" => {
                let n = py::to_int(&a["amount"]).filter(|_| py::truthy(&a["amount"])).unwrap_or(3).clamp(1, 20);
                let d = if py::truthy(&a["direction"]) { py::str_of(&a["direction"]) } else { "down".into() };
                let dy = match d.as_str() {
                    "up" => n,
                    "down" => -n,
                    _ => 0,
                };
                let dx = match d.as_str() {
                    "left" => n,
                    "right" => -n,
                    _ => 0,
                };
                macdesk::call("scroll", json!([a["x"], a["y"], dy, dx])).await.map_err(m)?;
            }
            "type" => {
                macdesk::call("type_text", json!([a["text"]])).await.map_err(m)?;
            }
            "key" => {
                macdesk::call("press_seq", json!([a["keys"]])).await.map_err(m)?;
            }
            "open_app" => {
                let e = macdesk::call("open_app", json!([a["name"]])).await.map_err(m)?;
                if py::truthy(&e) {
                    return err(py::str_of(&e));
                }
            }
            _ => return err(format!("未知操作 {op}")),
        }
        Ok(json!({}))
    }

    async fn desk_run(&self, op: &str, a: &Value) -> R<Value> {
        match self.place {
            Place::Local => self.desk_local(op, a).await,
            Place::Mac(_) => self.call("desk", json!({"action": op, "a": a})).await,
        }
    }

    // ---------- 文件：只在工作文件夹里 ----------
    fn workspace_str(&self) -> String {
        self.root.display().to_string()
    }

    /// 模型或界面给的路径 → 工作文件夹里的真实路径
    pub fn path(&self, path: &str, follow: bool) -> R<PathBuf> {
        let ws = self.workspace_str();
        let mut p = path.trim().to_string();
        if p.is_empty() {
            p = ws.clone();
        }
        if p == LEGACY || p.starts_with(&format!("{LEGACY}/")) {
            p = format!("{ws}{}", &p[LEGACY.len()..]);
        }
        let q = config::expand_user(Path::new(&p));
        let q = if q.is_absolute() { q } else { self.root.join(q) };
        let q = PathBuf::from(util::normpath(&q.display().to_string()));
        let r = if follow {
            resolve(&q)
        } else {
            match (q.parent(), q.file_name()) {
                (Some(par), Some(n)) => resolve(par).join(n),
                _ => resolve(&q),
            }
        };
        if r != self.root && !r.starts_with(&self.root) {
            return err(t!("只能访问工作文件夹里的文件（{dir}）", dir = ws));
        }
        Ok(r)
    }

    pub async fn read_local(&self, path: &str, limit: usize) -> R<Vec<u8>> {
        let p = self.path(path, true)?;
        if !p.is_file() {
            return err(t!("文件不存在：{path}", path = path));
        }
        if std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0) as usize > limit {
            return err(t!("文件太大"));
        }
        tokio::fs::read(&p).await.map_err(|e| ComputerError(e.to_string()))
    }

    pub async fn read_range_local(&self, path: &str, offset: u64, length: u64) -> R<Vec<u8>> {
        let p = self.path(path, true)?;
        if !p.is_file() {
            return err(t!("文件不存在：{path}", path = path));
        }
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&p)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = Vec::new();
            f.take(length).read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await
        .map_err(|e| ComputerError(e.to_string()))?
        .map_err(|e| ComputerError(e.to_string()))
    }

    pub async fn write_local(&self, path: &str, data: &[u8]) -> R<()> {
        let p = self.path(path, true)?;
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        tokio::fs::write(&p, data).await.map_err(|e| ComputerError(e.to_string()))
    }

    pub fn ls_local(&self, path: &str) -> Result<Vec<Value>, String> {
        let p = self.path(path, true).map_err(|e| e.0)?;
        if !p.is_dir() {
            return Err(t!("目录不存在"));
        }
        let mut out = vec![];
        for e in std::fs::read_dir(&p).map_err(|e| e.to_string())?.take(1000).flatten() {
            let Ok(st) = std::fs::symlink_metadata(e.path()) else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            out.push(json!({"name": name, "path": super::base::join(path, &name), "dir": st.is_dir(), "size": st.len(), "mtime": mtime(&st)}));
        }
        Ok(out)
    }

    pub fn stat_local(&self, paths: &[String]) -> IndexMap<String, Value> {
        let mut out = IndexMap::new();
        for x in paths {
            let Ok(p) = self.path(x, false) else { continue };
            let Ok(st) = std::fs::symlink_metadata(&p) else { continue };
            out.insert(x.clone(), json!({"name": super::base::basename(x), "path": x, "dir": st.is_dir(), "size": st.len(), "mtime": mtime(&st)}));
        }
        out
    }

    pub fn rm_local(&self, path: &str) -> Result<(), String> {
        let p = self.path(path, false).map_err(|e| e.0)?;
        if p == self.root {
            return Err(t!("不能删除整个工作目录"));
        }
        let md = std::fs::symlink_metadata(&p);
        match md {
            Ok(m) if m.file_type().is_symlink() || m.is_file() => std::fs::remove_file(&p).map_err(|e| e.to_string()),
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p).map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }

    pub fn upload_local(&self, paths: Vec<String>) -> R<Vec<String>> {
        let mut out = vec![];
        for x in paths {
            let p = self.path(&x, true)?;
            if !p.is_file() {
                return err(t!("文件不存在：{path}", path = x));
            }
            out.push(p.display().to_string());
        }
        Ok(out)
    }

    // ---------- 终端 ----------
    /// 在这台电脑上执行一条命令（工作目录是工作文件夹）
    pub async fn run(&self, cmd: &str, timeout: i64, stdin: Option<&str>) -> R<ExecResult> {
        let sh = which("bash").unwrap_or_else(|| "/bin/sh".into());
        let mut c = tokio::process::Command::new(sh);
        c.arg("-c").arg(cmd).current_dir(&self.root).env_clear().envs(clean_env());
        c.stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        c.process_group(0);
        let mut p = c.spawn().map_err(|e| ComputerError(e.to_string()))?;
        let pid = p.id();
        if let (Some(s), Some(text)) = (p.stdin.take(), stdin) {
            let text = text.to_string();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut s = s;
                let _ = s.write_all(text.as_bytes()).await;
            });
        }
        match tokio::time::timeout(std::time::Duration::from_secs(timeout.max(1) as u64), p.wait_with_output()).await {
            Ok(Ok(o)) => Ok(ExecResult::new(
                o.status.code().unwrap_or(-1) as i64,
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr),
                false,
            )),
            Ok(Err(e)) => err(e.to_string()),
            Err(_) => {
                #[cfg(unix)]
                if let Some(pid) = pid {
                    unsafe {
                        libc::killpg(pid as i32, libc::SIGKILL);
                    }
                }
                Ok(ExecResult::new(124, "", t!("超时"), true))
            }
        }
    }
}

fn mtime(m: &std::fs::Metadata) -> f64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file()).map(|p| p.display().to_string())
}

/// Path.resolve()：链接都化开；不存在的部分原样接上
fn resolve(p: &Path) -> PathBuf {
    if let Ok(r) = std::fs::canonicalize(p) {
        return r;
    }
    let mut tail = vec![];
    let mut cur = p.to_path_buf();
    loop {
        match (cur.parent().map(|x| x.to_path_buf()), cur.file_name().map(|x| x.to_os_string())) {
            (Some(par), Some(n)) => {
                tail.push(n);
                if let Ok(r) = std::fs::canonicalize(&par) {
                    let mut out = r;
                    for t in tail.iter().rev() {
                        out.push(t);
                    }
                    return out;
                }
                cur = par;
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn b64(v: &Value) -> R<Vec<u8>> {
    util::b64_decode(v.as_str().unwrap_or("")).ok_or_else(|| ComputerError("base64".into()))
}

#[async_trait]
impl Backend for Host {
    fn kind(&self) -> &'static str {
        match self.place {
            Place::Local => "host",
            Place::Mac(_) => "mac",
        }
    }
    fn workspace(&self) -> String {
        match self.place {
            Place::Local => self.workspace_str(),
            Place::Mac(l) => link::connected(l).map(|lk| lk.workspace.clone()).unwrap_or_default(),
        }
    }
    fn auto_pause(&self) -> bool {
        false
    }
    async fn status(&self, _c: &Computer) -> Value {
        match self.place {
            Place::Local => self.status_local().await,
            Place::Mac(l) => {
                if link::connected(l).is_none() {
                    return json!({"state": "error", "detail": link::offline(&self.mac_name())});
                }
                match self.call("status", json!({})).await {
                    Ok(v) => v,
                    Err(e) => json!({"state": "error", "detail": e.0}),
                }
            }
        }
    }
    async fn start(&self, c: &Computer) -> R<()> {
        check_enabled()?;
        let st = self.status(c).await;
        if st["state"] == "error" {
            return err(py::str_of(&st["detail"]));
        }
        match self.place {
            Place::Local => {
                let _g = self.starting.lock().await;
                if self.status(c).await["state"] != "running" {
                    let _ = std::fs::create_dir_all(&self.profile);
                    self.helper_local(c, &json!({"op": "ensure"})).await?;
                    c.changed();
                }
            }
            Place::Mac(_) => {
                if st["state"] != "running" {
                    self.call_t("start", 120.0, json!({})).await?;
                    c.changed();
                }
            }
        }
        Ok(())
    }
    async fn pause(&self, c: &Computer) -> R<()> {
        match self.place {
            Place::Local => {
                if self.status(c).await["state"] == "running" {
                    self.helper_local(c, &json!({"op": "quit"})).await?;
                    for _ in 0..20 {
                        if self.status(c).await["state"] != "running" {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                }
                Ok(())
            }
            Place::Mac(l) => {
                if link::connected(l).is_some() {
                    self.call("pause", json!({})).await?;
                }
                Ok(())
            }
        }
    }
    async fn exec(&self, _c: &Computer, cmd: &str, timeout: i64, stdin: Option<&str>) -> R<ExecResult> {
        match self.place {
            Place::Local => {
                if !shell_on() {
                    return err(t!("本机电脑的终端没有打开（电脑面板里可以打开，打开后每条命令都要你确认）"));
                }
                self.run(cmd, timeout, stdin).await
            }
            Place::Mac(_) => {
                if !self.shell_enabled() {
                    let lk = self.link()?;
                    if !lk.can_shell {
                        return err(t!("「{name}」上的 App 太旧，不能执行终端命令，更新到新版再试", name = self.mac_name()));
                    }
                    return err(t!("「{name}」的终端没有打开（电脑面板里可以打开，手机上也行，打开后每条命令都要你确认）", name = self.mac_name()));
                }
                let r = self.call_t("exec", (timeout + 15) as f64, json!({"cmd": cmd, "secs": timeout, "stdin": stdin})).await?;
                Ok(ExecResult::new(
                    py::to_int(&r["exit_code"]).unwrap_or(-1),
                    py::str_of(&r["stdout"]),
                    py::str_of(&r["stderr"]),
                    py::truthy(&r["timed_out"]),
                ))
            }
        }
    }
    async fn open_vnc(&self, _c: &Computer) -> R<Vnc> {
        err(t!("本机电脑没有远程画面，浏览器窗口就在这台电脑的屏幕上"))
    }

    async fn read_bytes(&self, _c: &Computer, path: &str, limit: usize) -> R<Vec<u8>> {
        match self.place {
            Place::Local => self.read_local(path, limit).await,
            Place::Mac(_) => b64(&self.call("read", json!({"path": path, "limit": limit})).await?),
        }
    }
    async fn read_range(&self, c: &Computer, path: &str, offset: u64, length: u64) -> R<Vec<u8>> {
        match self.place {
            Place::Local => self.read_range_local(path, offset, length).await,
            Place::Mac(_) => {
                if self.link()?.can_range {
                    return b64(&self.call("read", json!({"path": path, "offset": offset, "length": length})).await?);
                }
                let _g = self.whole_lock.lock().await;
                let (p, ts, _) = {
                    let d = self.desk.lock();
                    (d.whole.0.clone(), d.whole.1, ())
                };
                if p != path || py::now() - ts > 120.0 {
                    self.desk.lock().whole = (String::new(), 0.0, vec![]);
                    let data = self.read_bytes(c, path, OLD_MAX).await?;
                    self.desk.lock().whole = (path.to_string(), py::now(), data);
                }
                let d = self.desk.lock();
                let data = &d.whole.2;
                let a = (offset as usize).min(data.len());
                let z = (a + length as usize).min(data.len());
                Ok(data[a..z].to_vec())
            }
        }
    }
    async fn write_bytes(&self, _c: &Computer, path: &str, data: &[u8]) -> R<()> {
        match self.place {
            Place::Local => self.write_local(path, data).await,
            Place::Mac(_) => self.call("write", json!({"path": path, "data": util::b64(data)})).await.map(|_| ()),
        }
    }
    async fn ls(&self, _c: &Computer, path: &str) -> Result<Vec<Value>, String> {
        match self.place {
            Place::Local => self.ls_local(path),
            Place::Mac(_) => self.call("ls", json!({"path": path})).await.map(|v| v.as_array().cloned().unwrap_or_default()).map_err(|e| e.0),
        }
    }
    async fn stat_paths(&self, _c: &Computer, paths: &[String]) -> R<IndexMap<String, Value>> {
        match self.place {
            Place::Local => Ok(self.stat_local(paths)),
            Place::Mac(_) => {
                let v = self.call("stat", json!({"paths": paths})).await?;
                Ok(v.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default())
            }
        }
    }
    async fn rm(&self, _c: &Computer, path: &str) -> Result<(), String> {
        match self.place {
            Place::Local => self.rm_local(path),
            Place::Mac(_) => self.call("rm", json!({"path": path})).await.map(|_| ()).map_err(|e| e.0),
        }
    }
    fn upload_paths(&self, paths: Vec<String>) -> R<Vec<String>> {
        match self.place {
            Place::Local => self.upload_local(paths),
            Place::Mac(_) => Ok(paths),
        }
    }
    async fn helper_run(&self, c: &Computer, payload: &Value) -> R<Value> {
        match self.place {
            Place::Local => self.helper_local(c, payload).await,
            Place::Mac(_) => self.call("helper", json!({"payload": payload})).await,
        }
    }
    async fn look(&self, c: &Computer) -> R<(Vec<u8>, String)> {
        if self.show_screen(c, "").await {
            let png = util::b64_decode(&self.capture("png", 70).await?).unwrap_or_default();
            let f = self.front_app().await?;
            let (app, title) = (py::str_of(&f["app"]), f["title"].as_str().unwrap_or("").to_string());
            return Ok((png, if title.is_empty() { app } else { format!("{app} — {title}") }));
        }
        let r = self.peek_page(c, "png").await?;
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        Ok((b64(&r["data"])?, format!("{} | {}", s("title"), s("url"))))
    }
    async fn screenshot(&self, c: &Computer) -> R<Vec<u8>> {
        Ok(self.look(c).await?.0)
    }
    async fn type_text(&self, c: &Computer, text: &str, as_human: bool) -> R<()> {
        if !text.is_empty() {
            c.browser("type", None, as_human, json!({"text": text})).await?;
        }
        Ok(())
    }
    async fn press(&self, c: &Computer, keys: &[String], as_human: bool) -> R<()> {
        if keys.is_empty() {
            return err(t!("没有按键"));
        }
        let ks: Vec<String> = keys
            .iter()
            .map(|k| match k.to_lowercase().as_str() {
                "return" => "enter".to_string(),
                "backspace" => "backspace".to_string(),
                _ => k.clone(),
            })
            .collect();
        c.browser("press", None, as_human, json!({"keys": ks.join("+")})).await?;
        Ok(())
    }
    async fn shell(&self, c: &Computer, cmd: &str, timeout: i64) -> R<ExecResult> {
        c.touch();
        self.exec(c, cmd, timeout.clamp(1, SHELL_MAX_S), None).await
    }
    async fn peek(&self, c: &Computer, fmt: &str, view: &str) -> R<Vec<u8>> {
        if self.show_screen(c, view).await {
            return util::b64_decode(&self.capture(fmt, 60).await?).ok_or_else(|| ComputerError("base64".into()));
        }
        b64(&self.peek_page(c, fmt).await?["data"])
    }
    fn on_handback(&self, _c: &Computer) {
        let mut d = self.desk.lock();
        d.paused_by.clear();
        if d.at != 0.0 {
            d.at = py::now();
        }
    }
    async fn on_release(&self, c: &Computer, key: &str) {
        self.desk_release(c, key).await;
    }

    // ---------- 本机电脑 / 派到的 Mac ----------
    fn shell_enabled(&self) -> bool {
        match self.place {
            Place::Local => shell_on(),
            Place::Mac(l) => {
                let lk = link::connected(l);
                let r = link::row(l);
                lk.is_some_and(|lk| lk.can_shell) && r.is_some_and(|r| r["shell"] == true)
            }
        }
    }
    fn desktop_enabled(&self) -> bool {
        match self.place {
            Place::Local => {
                if !(host_desktop() && security::local_exec() && macdesk::supported()) {
                    return false;
                }
                let p = perm_cache::get();
                p.is_some_and(|p| py::truthy(&p["accessibility"]) && py::truthy(&p["screen"]))
            }
            Place::Mac(l) => {
                let Some(lk) = link::connected(l) else { return false };
                if !(lk.desktop && host_desktop()) {
                    return false;
                }
                let cached = self.desk.lock().info.1.clone();
                let info = if py::truthy(&cached) { cached } else { lk.perms.clone() };
                py::truthy(&info["accessibility"]) && py::truthy(&info["screen"])
            }
        }
    }
    fn desk_supported(&self) -> bool {
        match self.place {
            Place::Local => macdesk::supported() && security::local_exec(),
            Place::Mac(l) => link::connected(l).is_some_and(|lk| lk.desktop),
        }
    }
    fn desk_busy(&self) -> bool {
        self.held("")
    }
    async fn desk_state(&self, _c: &Computer) -> Value {
        if !self.desk_supported() {
            return json!({"supported": false});
        }
        let busy = self.desk_busy();
        let mut out = {
            let d = self.desk.lock();
            json!({"supported": true, "paused_by": d.paused_by, "owner": if busy { d.label.clone() } else { String::new() }})
        };
        match self.desk_info(false).await {
            Err(e) => {
                out["problem"] = json!(e.0);
            }
            Ok(info) => {
                for k in ["accessibility", "screen"] {
                    out[k] = json!(py::truthy(&info[k]));
                }
                out["problem"] = json!(self.desk_problem(&info, false));
            }
        }
        out
    }
    async fn show(&self, c: &Computer) -> R<()> {
        c.start().await?;
        let watch = {
            let s = c.st.lock();
            s.watch.clone().filter(|w| s.users.get(w).is_some_and(|u| py::truthy(&u.window)))
        };
        match watch {
            Some(w) => c.front(&w).await,
            None => c.browser("front", None, true, json!({})).await.map(|_| ()),
        }
    }
    async fn grant(&self, _c: &Computer) -> R<()> {
        match self.place {
            Place::Local => macdesk::request_permissions().await.map(|_| ()).map_err(ComputerError),
            Place::Mac(_) => self.call_t("grant", 30.0, json!({})).await.map(|_| ()),
        }
    }
    /// Bot 在屏幕上做一个动作（先 desk_claim）
    async fn desk(&self, c: &Computer, op: &str, key: &str, args: &Value) -> R<Value> {
        if self.desk.lock().owner != key {
            return err("没有占住屏幕");
        }
        if c.wait_for_control().await? {
            return err(DESK_STALE);
        }
        let info = self.desk_info(true).await?;
        let why = self.desk_problem(&info, false);
        if !why.is_empty() {
            return err(why);
        }
        if self.user_active(&info) {
            self.desk.lock().paused_by = "input".into();
            c.takeover();
            c.changed();
            let _ = c.wait_for_control().await;
            return err(DESK_STALE);
        }
        let _l = c.lock.lock().await;
        let r = self.desk_run(op, args).await;
        let now = py::now();
        {
            let mut d = self.desk.lock();
            d.at = now;
            d.seen = now;
            if d.ok.contains_key(key) {
                d.ok.insert(key.to_string(), now);
            }
        }
        c.touch();
        let first = {
            let mut s = c.st.lock();
            let was = s.recent_desk;
            s.recent_desk = true;
            !was
        };
        if first {
            c.changed();
        }
        r
    }
    async fn desk_claim(&self, c: &Computer, key: &str, label: &str, on_wait: &(dyn Fn(String) + Sync)) -> R<()> {
        if self.held(key) {
            if key.starts_with("chat-") {
                let l = self.desk.lock().label.clone();
                return err(format!("屏幕正被{l}用着（鼠标键盘一次只能一个任务用）。等它做完，或者先 cancel_task 停掉它。"));
            }
            on_wait(t!("排队等屏幕（{who} 正在用）", who = self.desk.lock().label.clone()));
            while self.held(key) {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.desk_free.notified()).await;
            }
        }
        let changed = {
            let mut d = self.desk.lock();
            let ch = d.owner != key;
            if ch {
                d.owner = key.into();
                d.label = label.into();
                d.at = 0.0;
            }
            d.seen = py::now();
            ch
        };
        if changed {
            c.changed();
        }
        Ok(())
    }
    async fn desk_release(&self, c: &Computer, key: &str) {
        {
            let mut d = self.desk.lock();
            if d.owner != key {
                return;
            }
            d.owner.clear();
            d.label.clear();
        }
        self.desk_free.notify_waiters();
        c.changed();
    }
    fn desk_allowed(&self, key: &str) -> bool {
        let d = self.desk.lock();
        d.ok.get(key).is_some_and(|t| !key.starts_with("chat-") || py::now() - t < CHAT_OK)
    }
    fn desk_ok(&self, key: &str) {
        self.desk.lock().ok.insert(key.into(), py::now());
    }
    fn desk_denied(&self, key: &str) -> Option<f64> {
        self.desk.lock().denied.get(key).copied()
    }
    fn set_desk_denied(&self, key: &str) {
        self.desk.lock().denied.insert(key.into(), py::now());
    }
    fn desk_owner(&self) -> String {
        self.desk.lock().owner.clone()
    }
    fn lid(&self) -> i64 {
        self.lid_()
    }
    fn name(&self) -> String {
        match self.place {
            Place::Mac(_) => self.mac_name(),
            Place::Local => String::new(),
        }
    }
}

/// desktop_enabled 是同步的（工具列表每轮都要），本机要问 macdesk 的两个权限：问到的留一秒
mod perm_cache {
    use parking_lot::Mutex;
    use serde_json::Value;
    static CACHE: Mutex<(f64, Option<Value>)> = Mutex::new((0.0, None));

    pub fn get() -> Option<Value> {
        let (at, v) = CACHE.lock().clone();
        if crate::py::now() - at <= 1.0 {
            return v;
        }
        let Ok(h) = tokio::runtime::Handle::try_current() else { return None };
        let p = tokio::task::block_in_place(|| h.block_on(super::macdesk::permissions())).ok();
        *CACHE.lock() = (crate::py::now(), p.clone());
        p
    }

    pub fn remember(p: &Value) {
        *CACHE.lock() = (crate::py::now(), Some(p.clone()));
    }
}
