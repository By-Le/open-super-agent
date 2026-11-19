//! 电脑接口。所有 Bot 共用一台电脑；后端只需实现执行命令、读写文件、启动/暂停和 VNC 连接。对应 osa/computer/base.py。
//!
//! 几个任务可以同时用：每个任务在自己的浏览器窗口里做（登录状态、文件共用），操作都经 CDP 发给自己窗口里的页面（cdp_helper.py，
//! 在电脑里面跑），不动真实的鼠标键盘。同时用的任务最多 computer.parallel 个，多了排队。
//!
//! 结构：`Computer` 是所有后端共用的部分（谁在用、排队、窗口、接手 / 交还、浏览器操作），`Backend` 是每种电脑自己的部分。
//! Backend 的方法有默认实现的（读写文件、截图、打字……）走终端命令，本机电脑（host，第 2 步 B）按自己的办法重写。

use crate::{config, py};
use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio::io::{AsyncRead, AsyncWrite};

pub static HELPER: &[u8] = include_bytes!("../../../../../osa/computer/cdp_helper.py");
pub static HELPER_PATH: LazyLock<String> = LazyLock::new(|| {
    use sha1::Digest;
    let h: String = sha1::Sha1::digest(HELPER).iter().map(|b| format!("{b:02x}")).collect();
    format!("/tmp/osa-cdp-{}.py", &h[..8])
});

pub const DISPLAY: &str = ":1";
pub const HOSTS: [&str; 2] = ["host", "mac"];
pub const SHELL_MAX_S: i64 = 600;
pub const CHAT_KEEP: f64 = 600.0;
pub const STALE_OPS: [&str; 9] = ["click", "type", "key", "upload", "scroll", "back", "click_xy", "press", "wheel"];
pub const STALE: &str = "用户刚接手过电脑，页面可能已经变了，这一步没有执行。先看一下页面（browser_snapshot 或 computer_screenshot）再继续";

#[derive(Debug, Clone)]
pub struct ComputerError(pub String);

impl std::fmt::Display for ComputerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ComputerError {}

impl From<String> for ComputerError {
    fn from(s: String) -> Self {
        ComputerError(s)
    }
}

pub type R<T> = Result<T, ComputerError>;

pub fn err<T>(s: impl Into<String>) -> R<T> {
    Err(ComputerError(s.into()))
}

/// config.yaml 里 computer.enabled: false（比如演示服务器）：电脑不开机
pub fn check_enabled() -> R<()> {
    if !config::get_bool("computer.enabled", true) {
        return err(t!("这台服务器没有打开电脑功能（config.yaml 的 computer.enabled）"));
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl ExecResult {
    pub fn new(exit_code: i64, stdout: impl Into<String>, stderr: impl Into<String>, timed_out: bool) -> Self {
        ExecResult { exit_code, stdout: stdout.into(), stderr: stderr.into(), timed_out }
    }

    pub fn text(&self, limit: usize) -> String {
        let mut out = self.stdout.clone();
        if !self.stderr.trim().is_empty() {
            out += &format!("\n[stderr]\n{}", self.stderr);
        }
        let n = py::char_len(&out);
        if n > limit {
            out = format!("{}\n…（输出过长，截断了 {} 字）", py::head(&out, limit), n - limit);
        }
        let tail = if self.timed_out { "（超时）" } else { "" };
        format!("退出码 {}{tail}\n{out}", self.exit_code).trim().to_string()
    }
}

/// shlex.quote
pub fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

pub fn keysym(key: &str) -> R<String> {
    if key.is_empty() || key.chars().any(|c| c.is_whitespace()) {
        return err(t!("按键名不合法：{key}", key = py::repr(&json!(key))));
    }
    let low = key.to_lowercase();
    let m = match low.as_str() {
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" => "space",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Prior",
        "pagedown" => "Next",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "ctrl" | "control" => "ctrl",
        "alt" => "alt",
        "shift" => "shift",
        "meta" | "cmd" | "super" | "win" => "super",
        "+" => "plus",
        _ => "",
    };
    if !m.is_empty() {
        return Ok(m.into());
    }
    if let Some(n) = low.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()) {
        if (1..=24).contains(&n) && low[1..].chars().all(|c| c.is_ascii_digit()) {
            return Ok(low.to_uppercase());
        }
    }
    Ok(key.to_string())
}

/// 网页上的实时画面：VNC（RFB）字节流的两头
pub enum Vnc {
    Io { rx: Box<dyn AsyncRead + Unpin + Send>, tx: Box<dyn AsyncWrite + Unpin + Send>, child: Option<tokio::process::Child> },
    Ws(Box<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>),
}

/// 每种电脑自己的部分。带默认实现的方法是“走终端命令”的做法，本机电脑（host）按自己的办法重写
#[async_trait]
pub trait Backend: Send + Sync {
    fn kind(&self) -> &'static str;
    /// 文件放在这里：资料库、上传的附件、Bot 下载的东西
    fn workspace(&self) -> String {
        "/data/workspace".into()
    }
    /// 闲置一段时间自动暂停（本机电脑不暂停）
    fn auto_pause(&self) -> bool {
        true
    }
    /// {"state": "none|starting|running|paused|stopped|error", "detail": str, …}
    async fn status(&self, c: &Computer) -> Value;
    /// 确保电脑在运行、桌面已启动。可以重复调用
    async fn start(&self, c: &Computer) -> R<()>;
    async fn pause(&self, c: &Computer) -> R<()>;
    async fn exec(&self, c: &Computer, cmd: &str, timeout: i64, stdin: Option<&str>) -> R<ExecResult>;
    async fn open_vnc(&self, c: &Computer) -> R<Vnc>;

    // ---- 有默认做法的（走终端命令） ----
    async fn read_bytes(&self, c: &Computer, path: &str, limit: usize) -> R<Vec<u8>> {
        c.read_bytes_sh(path, limit).await
    }
    async fn read_range(&self, c: &Computer, path: &str, offset: u64, length: u64) -> R<Vec<u8>> {
        c.read_range_sh(path, offset, length).await
    }
    async fn write_bytes(&self, c: &Computer, path: &str, data: &[u8]) -> R<()> {
        c.write_bytes_sh(path, data).await
    }
    async fn ls(&self, c: &Computer, path: &str) -> Result<Vec<Value>, String> {
        c.ls_sh(path).await
    }
    async fn stat_paths(&self, c: &Computer, paths: &[String]) -> R<IndexMap<String, Value>> {
        c.stat_paths_sh(paths).await
    }
    async fn rm(&self, c: &Computer, path: &str) -> Result<(), String> {
        c.rm_sh(path).await
    }
    fn upload_paths(&self, paths: Vec<String>) -> Vec<String> {
        paths
    }
    async fn helper_run(&self, c: &Computer, payload: &Value) -> R<Value> {
        c.helper_run_sh(payload).await
    }
    async fn look(&self, c: &Computer) -> R<(Vec<u8>, String)> {
        c.look_sh().await
    }
    async fn screenshot(&self, c: &Computer) -> R<Vec<u8>> {
        c.screenshot_sh().await
    }
    async fn type_text(&self, c: &Computer, text: &str, as_human: bool) -> R<()> {
        c.type_sh(text, as_human).await
    }
    async fn press(&self, c: &Computer, keys: &[String], as_human: bool) -> R<()> {
        c.press_sh(keys, as_human).await
    }
    async fn shell(&self, c: &Computer, cmd: &str, timeout: i64) -> R<ExecResult> {
        self.start(c).await?;
        c.touch();
        self.exec(c, cmd, timeout.clamp(1, SHELL_MAX_S), None).await
    }
    /// 界面上的画面（本机电脑没有 VNC，用截图代替）
    async fn peek(&self, _c: &Computer, _fmt: &str, _view: &str) -> R<Vec<u8>> {
        err(t!("这台电脑用实时画面，不用截图"))
    }
    /// 你接手 / 交还时，后端自己要做的（Instabox 告诉它自带的工具停手）
    fn on_takeover(&self, _c: &Computer) {}
    fn on_handback(&self, _c: &Computer) {}
    /// 一个任务（或研究员）不再用电脑了，后端自己要做的（本机电脑：让出整个屏幕）
    async fn on_release(&self, _c: &Computer, _key: &str) {}

    // ---- Instabox：选账号下已有的机器 ----
    async fn list_machines(&self, _c: &Computer) -> Option<R<Vec<Value>>> {
        None
    }
    fn use_machine(&self, _c: &Computer, _mid: &str) -> Option<R<()>> {
        None
    }
    fn fixed_machine(&self) -> bool {
        false
    }

    // ---- 本机电脑 / 派到的 Mac（HOSTS，第 2 步 B 实现）：终端开关、整个屏幕 ----
    fn shell_enabled(&self) -> bool {
        false
    }
    fn desktop_enabled(&self) -> bool {
        false
    }
    fn desk_supported(&self) -> bool {
        false
    }
    fn desk_busy(&self) -> bool {
        false
    }
    async fn desk_state(&self, _c: &Computer) -> Value {
        json!({"supported": false})
    }
    async fn show(&self, _c: &Computer) -> R<()> {
        Ok(())
    }
    async fn grant(&self, _c: &Computer) -> R<()> {
        err(t!("只有在 Mac 上用本机电脑时才能操作整台电脑"))
    }
    /// 整个屏幕的一个动作（desktop_*）：screenshot / inspect / focused / click / drag / type / key / scroll / open_app
    async fn desk(&self, _c: &Computer, _op: &str, _key: &str, _args: &Value) -> R<Value> {
        err("这台电脑现在不能操作整个屏幕")
    }
    async fn desk_claim(&self, _c: &Computer, _key: &str, _label: &str, _on_wait: &(dyn Fn(String) + Sync)) -> R<()> {
        Ok(())
    }
    async fn desk_release(&self, _c: &Computer, _key: &str) {}
    fn desk_allowed(&self, _key: &str) -> bool {
        false
    }
    fn desk_ok(&self, _key: &str) {}
    fn desk_denied(&self, _key: &str) -> Option<f64> {
        None
    }
    fn set_desk_denied(&self, _key: &str) {}
    /// 派到的 Mac：登记的编号（mac 后端用）
    fn lid(&self) -> i64 {
        0
    }
    fn name(&self) -> String {
        String::new()
    }
}

/// 谁在用电脑（一个任务、Heavy 的一个研究员）
#[derive(Clone, Debug, Default)]
pub struct User {
    pub label: String,
    pub since: f64,
    pub window: Value,
    pub browser: Value,
    pub task_id: Value,
    pub bot_id: Value,
}

#[derive(Clone, Debug, Default)]
pub struct ChatWin {
    pub window: Value,
    pub browser: Value,
    pub at: f64,
}

#[derive(Default)]
pub struct State {
    pub controller: String,
    pub last_used: f64,
    pub width: i64,
    pub height: i64,
    pub users: IndexMap<String, User>,
    pub waiting: IndexMap<String, String>,
    pub exclusive: String,
    pub exclusive_label: String,
    pub watch: Option<String>,
    pub chat_windows: HashMap<String, ChatWin>,
    pub asking: HashMap<String, String>,
    pub recent: (Value, Value),
    pub recent_desk: bool,
    pub helper_ok: bool,
}

pub struct Computer {
    pub bot_id: i64,
    pub backend: Box<dyn Backend>,
    pub st: Mutex<State>,
    handback_tx: tokio::sync::watch::Sender<bool>,
    /// 真实的鼠标键盘同一时间只做一件事
    pub lock: tokio::sync::Mutex<()>,
    free: tokio::sync::Notify,
    assign: tokio::sync::Mutex<()>,
    helper: tokio::sync::Mutex<()>,
    pub on_change: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn s(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        v => py::str_of(v),
    }
}

impl Computer {
    pub fn new(bot_id: i64, backend: Box<dyn Backend>, on_change: Option<Arc<dyn Fn() + Send + Sync>>) -> Arc<Computer> {
        Arc::new(Computer {
            bot_id,
            backend,
            st: Mutex::new(State { controller: "bot".into(), last_used: py::now(), width: 1280, height: 800, ..Default::default() }),
            handback_tx: tokio::sync::watch::channel(true).0,
            lock: tokio::sync::Mutex::new(()),
            free: tokio::sync::Notify::new(),
            assign: tokio::sync::Mutex::new(()),
            helper: tokio::sync::Mutex::new(()),
            on_change,
        })
    }

    pub fn kind(&self) -> &'static str {
        self.backend.kind()
    }

    pub fn workspace(&self) -> String {
        self.backend.workspace()
    }

    pub fn controller(&self) -> String {
        self.st.lock().controller.clone()
    }

    pub fn touch(&self) {
        self.st.lock().last_used = py::now();
    }

    pub fn slots() -> usize {
        let v = config::get("computer.parallel");
        (if v.is_null() { 3 } else { py::to_int(&v).unwrap_or(3) }).max(1) as usize
    }

    /// 谁在用电脑，给排队提示和 Bot 看
    pub fn busy(&self) -> String {
        let s = self.st.lock();
        if !s.exclusive_label.is_empty() {
            return s.exclusive_label.clone();
        }
        s.users.values().map(|u| u.label.clone()).collect::<Vec<_>>().join(&t!("、"))
    }

    pub fn changed(&self) {
        if let Some(f) = &self.on_change {
            f();
        }
    }

    fn can_take(&self, key: &str) -> bool {
        let s = self.st.lock();
        s.users.contains_key(key) || (s.exclusive.is_empty() && s.users.len() < Self::slots())
    }

    /// 占一个位置；满了（或者在演示录制）就排队。同一个 key 重复占没关系
    pub async fn claim(&self, key: &str, label: &str, on_wait: Option<&(dyn Fn(String) + Sync)>, task_id: Value, bot_id: Value) {
        if self.st.lock().users.contains_key(key) {
            return;
        }
        if !self.can_take(key) {
            self.st.lock().waiting.insert(key.into(), label.into());
            self.changed();
            struct Unwait<'a>(&'a Computer, String);
            impl Drop for Unwait<'_> {
                fn drop(&mut self) {
                    self.0.st.lock().waiting.shift_remove(&self.1);
                    self.0.changed();
                }
            }
            let _g = Unwait(self, key.to_string());
            if let Some(w) = on_wait {
                w(t!("排队等电脑（{who} 正在用）", who = self.busy()));
            }
            loop {
                let n = self.free.notified();
                tokio::pin!(n);
                n.as_mut().enable();
                if self.can_take(key) {
                    break;
                }
                n.await;
            }
        }
        {
            let mut s = self.st.lock();
            s.users.entry(key.into()).or_insert(User { label: label.into(), since: py::now(), window: Value::Null, browser: Value::Null, task_id, bot_id });
        }
        self.changed();
    }

    /// 演示录制：没有任务在用时才能开始，录制期间任务都排队
    pub fn claim_exclusive(&self, key: &str, label: &str) -> R<()> {
        {
            let mut s = self.st.lock();
            if !s.exclusive.is_empty() || !s.users.is_empty() {
                drop(s);
                return err(t!("电脑正在被用着（{who}）", who = self.busy()));
            }
            s.exclusive = key.into();
            s.exclusive_label = label.into();
        }
        self.changed();
        Ok(())
    }

    pub async fn release(self: &Arc<Self>, key: &str) {
        let u = {
            let mut s = self.st.lock();
            let u = s.users.shift_remove(key);
            if s.exclusive == key {
                s.exclusive.clear();
                s.exclusive_label.clear();
            } else if u.is_none() {
                return;
            }
            if s.watch.as_deref() == Some(key) {
                s.watch = None;
            }
            u
        };
        self.free.notify_waiters();
        if let Some(u) = u.filter(|u| !u.window.is_null()) {
            let me = self.clone();
            tokio::spawn(async move { me.close_window(u.window, u.browser).await });
        }
        self.backend.on_release(self, key).await;
        self.changed();
    }

    /// 并行数改大了：叫醒排队的
    pub fn recheck(&self) {
        self.free.notify_waiters();
    }

    async fn close_window(self: Arc<Self>, window: Value, browser: Value) {
        let _a = self.assign.lock().await;
        let taken: Vec<Value> = {
            let s = self.st.lock();
            s.users.values().map(|u| u.window.clone()).chain(s.chat_windows.values().map(|c| c.window.clone())).collect()
        };
        if taken.contains(&window) {
            return;
        }
        let _ = self.helper_run(&json!({"op": "close_window", "window": window, "browser": browser, "keep": Self::slots()})).await;
    }

    // ---------- 委托给后端的 ----------
    pub async fn status(&self) -> Value {
        self.backend.status(self).await
    }
    pub async fn state(&self) -> String {
        self.status().await["state"].as_str().unwrap_or("").to_string()
    }
    pub async fn start(&self) -> R<()> {
        self.backend.start(self).await
    }
    pub async fn pause(&self) -> R<()> {
        self.backend.pause(self).await
    }
    pub async fn exec(&self, cmd: &str, timeout: i64, stdin: Option<&str>) -> R<ExecResult> {
        self.backend.exec(self, cmd, timeout, stdin).await
    }
    pub async fn read_bytes(&self, path: &str, limit: usize) -> R<Vec<u8>> {
        self.backend.read_bytes(self, path, limit).await
    }
    pub async fn read_range(&self, path: &str, offset: u64, length: u64) -> R<Vec<u8>> {
        self.backend.read_range(self, path, offset, length).await
    }
    pub async fn write_bytes(&self, path: &str, data: &[u8]) -> R<()> {
        self.backend.write_bytes(self, path, data).await
    }
    pub async fn ls(&self, path: &str) -> Result<Vec<Value>, String> {
        self.backend.ls(self, path).await
    }
    pub async fn stat_paths(&self, paths: &[String]) -> R<IndexMap<String, Value>> {
        self.backend.stat_paths(self, paths).await
    }
    pub async fn rm(&self, path: &str) -> Result<(), String> {
        self.backend.rm(self, path).await
    }
    pub async fn helper_run(&self, payload: &Value) -> R<Value> {
        self.backend.helper_run(self, payload).await
    }
    pub async fn look(&self) -> R<(Vec<u8>, String)> {
        self.backend.look(self).await
    }
    pub async fn screenshot(&self) -> R<Vec<u8>> {
        self.backend.screenshot(self).await
    }
    pub async fn type_text(&self, text: &str, as_human: bool) -> R<()> {
        self.backend.type_text(self, text, as_human).await
    }
    pub async fn press(&self, keys: &str, as_human: bool) -> R<()> {
        let ks: Vec<String> = keys.replace(' ', "").split('+').filter(|k| !k.is_empty()).map(String::from).collect();
        self.backend.press(self, &ks, as_human).await
    }
    pub async fn shell(&self, cmd: &str, timeout: i64) -> R<ExecResult> {
        self.backend.shell(self, cmd, timeout).await
    }
    pub async fn open_vnc(&self) -> R<Vnc> {
        self.backend.open_vnc(self).await
    }

    // ---------- 默认做法：终端命令 ----------
    pub async fn read_bytes_sh(&self, path: &str, limit: usize) -> R<Vec<u8>> {
        let q = quote(path);
        let r = self.exec(&format!("[ -f {q} ] || exit 3; base64 -w0 {q}"), 120, None).await?;
        if r.exit_code == 3 {
            return err(t!("文件不存在：{path}", path = path));
        }
        if r.exit_code != 0 {
            return err(if r.stderr.is_empty() { t!("读取失败") } else { r.stderr });
        }
        let data = crate::util::b64_decode(&r.stdout).unwrap_or_default();
        if data.len() > limit {
            return err(t!("文件太大"));
        }
        Ok(data)
    }

    pub async fn read_range_sh(&self, path: &str, offset: u64, length: u64) -> R<Vec<u8>> {
        let q = quote(path);
        let r = self.exec(&format!("[ -f {q} ] || exit 3; tail -c +{} {q} | head -c {length} | base64 -w0", offset + 1), 120, None).await?;
        if r.exit_code == 3 {
            return err(t!("文件不存在：{path}", path = path));
        }
        if r.exit_code != 0 {
            return err(if r.stderr.is_empty() { t!("读取失败") } else { r.stderr });
        }
        Ok(crate::util::b64_decode(&r.stdout).unwrap_or_default())
    }

    pub async fn write_bytes_sh(&self, path: &str, data: &[u8]) -> R<()> {
        let q = quote(path);
        let b = crate::util::b64(data);
        let r = self.exec(&format!("mkdir -p \"$(dirname {q})\" && base64 -d > {q}"), 120, Some(&b)).await?;
        if r.exit_code != 0 {
            return err(if r.stderr.is_empty() { t!("写入失败") } else { r.stderr });
        }
        Ok(())
    }

    pub async fn ls_sh(&self, path: &str) -> Result<Vec<Value>, String> {
        let q = quote(path);
        let r = self
            .exec(&format!("[ -d {q} ] || exit 3; find {q} -mindepth 1 -maxdepth 1 -printf '%y\\t%s\\t%T@\\t%f\\n' | head -n 1000"), 30, None)
            .await
            .map_err(|e| e.0)?;
        if r.exit_code == 3 {
            return Err(t!("目录不存在"));
        }
        if r.exit_code != 0 {
            let e = if r.stderr.is_empty() { t!("读取目录失败") } else { r.stderr };
            return Err(py::head(e.trim(), 200));
        }
        let mut out = vec![];
        for line in r.stdout.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() != 4 {
                continue;
            }
            let (kind, size, mtime, name) = (parts[0], parts[1], parts[2], parts[3]);
            out.push(json!({"name": name, "path": join(path, name), "dir": kind == "d",
                "size": if !size.is_empty() && size.chars().all(|c| c.is_ascii_digit()) { size.parse::<i64>().unwrap_or(0) } else { 0 },
                "mtime": mtime.parse::<f64>().unwrap_or(0.0)}));
        }
        Ok(out)
    }

    pub async fn stat_paths_sh(&self, paths: &[String]) -> R<IndexMap<String, Value>> {
        let qs = paths.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
        let r = self.exec(&format!("for f in {qs}; do [ -e \"$f\" ] && stat -c '%F\t%s\t%Y\t%n' \"$f\"; done; true"), 30, None).await?;
        let mut out = IndexMap::new();
        for line in r.stdout.lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() == 4 {
                let (kind, size, mtime, name) = (parts[0], parts[1], parts[2], parts[3]);
                out.insert(name.to_string(), json!({"name": basename(name), "path": name, "dir": kind == "directory",
                    "size": if !size.is_empty() && size.chars().all(|c| c.is_ascii_digit()) { size.parse::<i64>().unwrap_or(0) } else { 0 },
                    "mtime": mtime.parse::<f64>().unwrap_or(0.0)}));
            }
        }
        Ok(out)
    }

    pub async fn rm_sh(&self, path: &str) -> Result<(), String> {
        let r = self.exec(&format!("rm -rf -- {}", quote(path)), 60, None).await.map_err(|e| e.0)?;
        if r.exit_code != 0 {
            let e = if r.stderr.is_empty() { t!("删除失败") } else { r.stderr };
            return Err(py::head(e.trim(), 200));
        }
        Ok(())
    }

    // ---------- 控制权 ----------
    pub fn takeover(&self) {
        self.st.lock().controller = "human".into();
        let _ = self.handback_tx.send(false);
        self.backend.on_takeover(self);
    }

    pub fn handback(&self) {
        self.st.lock().controller = "bot".into();
        let _ = self.handback_tx.send(true);
        self.backend.on_handback(self);
    }

    /// 用户接手期间等用户交还。返回这次是不是真的等过
    pub async fn wait_for_control(&self) -> R<bool> {
        if *self.handback_tx.borrow() {
            return Ok(false);
        }
        let mut rx = self.handback_tx.subscribe();
        match tokio::time::timeout(std::time::Duration::from_secs(1800), rx.wait_for(|v| *v)).await {
            Ok(_) => Ok(true),
            Err(_) => err("TimeoutError"),
        }
    }

    // ---------- 真实的屏幕 ----------
    pub async fn desktop_sh(&self, cmd: &str, stdin: Option<&str>, timeout: i64, as_human: bool) -> R<ExecResult> {
        if !as_human && self.wait_for_control().await? {
            return err(STALE);
        }
        self.start().await?;
        let _l = self.lock.lock().await;
        self.touch();
        let r = self.exec(&format!("export DISPLAY={DISPLAY}; {cmd}"), timeout, stdin).await?;
        if r.exit_code != 0 {
            let e = if !r.stderr.is_empty() {
                r.stderr.clone()
            } else if !r.stdout.is_empty() {
                r.stdout.clone()
            } else {
                t!("退出码 {code}", code = r.exit_code)
            };
            return err(py::head(e.trim(), 500));
        }
        Ok(r)
    }

    pub async fn look_sh(&self) -> R<(Vec<u8>, String)> {
        let png = self.screenshot().await?;
        let title = self.exec(&format!("DISPLAY={DISPLAY} xdotool getactivewindow getwindowname 2>/dev/null"), 10, None).await?.stdout.trim().to_string();
        Ok((png, title))
    }

    pub async fn screenshot_sh(&self) -> R<Vec<u8>> {
        let path = format!("/tmp/osa-shot-{}.png", py::now_ns());
        self.desktop_sh(&format!("scrot -o {path} 2>/dev/null || import -window root png:{path}"), None, 60, true).await?;
        let r = self.read_bytes(&path, 32 << 20).await;
        let _ = self.exec(&format!("rm -f {path}"), 10, None).await;
        r
    }

    pub async fn type_sh(&self, text: &str, as_human: bool) -> R<()> {
        if text.is_empty() {
            return Ok(());
        }
        if text.is_ascii() {
            self.desktop_sh("xdotool type --delay 12 --file -", Some(text), 60, as_human).await?;
        } else {
            self.desktop_sh(
                "if command -v xclip >/dev/null; then xclip -selection clipboard -i && sleep 0.1 && xdotool key ctrl+v; else xdotool type --delay 30 --file -; fi",
                Some(text),
                60,
                as_human,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn press_sh(&self, keys: &[String], as_human: bool) -> R<()> {
        if keys.is_empty() {
            return err(t!("没有按键"));
        }
        let ks: Vec<String> = keys.iter().map(|k| keysym(k)).collect::<R<_>>()?;
        self.desktop_sh(&format!("xdotool key -- {}", quote(&ks.join("+"))), None, 60, as_human).await?;
        Ok(())
    }

    // ---------- 浏览器（CDP） ----------
    /// 在电脑里跑一次浏览器控制脚本。脚本第一次用时写进电脑；电脑重建过就重新写
    pub async fn helper_run_sh(&self, payload: &Value) -> R<Value> {
        let stdin = py::dumps_u(payload);
        for attempt in 0..2 {
            if !self.st.lock().helper_ok {
                let _h = self.helper.lock().await;
                if !self.st.lock().helper_ok {
                    if self.exec(&format!("test -f {}", *HELPER_PATH), 10, None).await?.exit_code != 0 {
                        self.write_bytes(&HELPER_PATH, HELPER).await?;
                    }
                    self.st.lock().helper_ok = true;
                }
            }
            let r = self.exec(&format!("export DISPLAY={DISPLAY}; python3 {}", *HELPER_PATH), 90, Some(&stdin)).await?;
            if r.exit_code == 0 {
                let last = r.stdout.trim().lines().last().unwrap_or("").to_string();
                return serde_json::from_str(&last).map_err(|e| ComputerError(e.to_string()));
            }
            let e = if !r.stderr.is_empty() {
                r.stderr.clone()
            } else if !r.stdout.is_empty() {
                r.stdout.clone()
            } else {
                t!("退出码 {code}", code = r.exit_code)
            };
            let e = e.trim().to_string();
            if attempt == 0 && (e.contains("No such file") || e.contains("can't open file")) {
                self.st.lock().helper_ok = false;
                continue;
            }
            let last = e.lines().last().unwrap_or("");
            return err(py::head(last.strip_prefix("RuntimeError: ").unwrap_or(last), 500));
        }
        err(t!("浏览器控制脚本运行失败"))
    }

    /// 告诉脚本这次在哪个窗口做（见 cdp_helper.Ctx）
    fn win(&self, key: &str, op: &str) -> Value {
        let s = self.st.lock();
        let now = py::now();
        let tasks: Vec<(String, Value)> = s.users.iter().filter(|(_, u)| py::truthy(&u.window)).map(|(k, u)| (k.clone(), u.window.clone())).collect();
        let chats: Vec<(String, Value)> = s
            .chat_windows
            .iter()
            .filter(|(_, c)| py::truthy(&c.window) && now - c.at < CHAT_KEEP)
            .map(|(k, c)| (k.clone(), c.window.clone()))
            .collect();
        // {**tasks, **chats}：同一个 key 后者盖前者
        let mut merged: IndexMap<String, Value> = IndexMap::new();
        for (k, w) in tasks.iter().chain(chats.iter()) {
            merged.insert(k.clone(), w.clone());
        }
        let avoid: Vec<Value> = merged.iter().filter(|(k, _)| *k != key).map(|(_, w)| w.clone()).collect();
        let alone = s.controller == "bot" && chats.is_empty() && op != "front";
        if let Some(u) = s.users.get(key) {
            let first = !py::truthy(&u.window);
            let watched = if s.watch.as_deref() != Some(key) {
                s.watch.as_ref().and_then(|w| s.users.get(w)).map(|w| w.window.clone()).unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            return json!({"id": u.window, "browser": u.browser, "avoid": avoid, "keep_front": watched,
                "front": first && !py::truthy(&watched) && s.controller == "bot",
                "restart_ok": first && alone && s.users.len() == 1});
        }
        let c = s.chat_windows.get(key).cloned().unwrap_or_default();
        let task_wins: Vec<&Value> = tasks.iter().map(|(_, w)| w).collect();
        let mine = if py::truthy(&c.window) && !task_wins.contains(&&c.window) { c.window.clone() } else { Value::Null };
        json!({"id": mine, "browser": if mine.is_null() { Value::Null } else { c.browser.clone() }, "avoid": avoid, "front": true, "router": true,
               "op": op, "restart_ok": alone && op == "open" && s.users.is_empty()})
    }

    fn set_window(&self, key: &str, window: Value, browser: Value) {
        let mut s = self.st.lock();
        if let Some(u) = s.users.get_mut(key) {
            u.window = window;
            u.browser = browser;
        } else if key.starts_with("chat-") {
            s.chat_windows.insert(key.into(), ChatWin { window, browser, at: py::now() });
        }
    }

    /// 用 CDP 操作浏览器。key 是谁在操作：任务（claim 过的）在自己的窗口里做；对话里直接操作的（chat-<Bot id>）在屏幕上看到的那个窗口里做。
    /// 不给 key 是老办法：最前面的标签页（演示录制）。as_human：用户自己发起的操作
    pub async fn browser(&self, op: &str, key: Option<&str>, as_human: bool, args: Value) -> R<Value> {
        if !as_human && self.wait_for_control().await? && STALE_OPS.contains(&op) {
            return err(STALE);
        }
        self.start().await?;
        self.touch();
        let mut payload = json!({"op": op});
        if let Some(o) = args.as_object() {
            for (k, v) in o {
                payload[k] = v.clone();
            }
        }
        let Some(key) = key else {
            return self.helper_run(&payload).await;
        };
        if !self.st.lock().users.contains_key(key) && !key.starts_with("chat-") {
            return err("这个任务已经不占用电脑了");
        }
        if key.starts_with("chat-") && self.st.lock().watch.is_some() {
            self.st.lock().watch = None;
            self.changed();
        }
        for attempt in 0..2 {
            let r: R<Value> = async {
                if self.win(key, op)["id"].is_null() {
                    let _a = self.assign.lock().await;
                    let w = self.win(key, op);
                    if w["id"].is_null() {
                        let got = self.helper_run(&json!({"op": "window", "win": w})).await?;
                        self.set_window(key, got["window"].clone(), got.get("browser").cloned().unwrap_or(Value::Null));
                        self.changed();
                    }
                }
                let mut p = payload.clone();
                p["win"] = self.win(key, op);
                self.helper_run(&p).await
            }
            .await;
            let out = match r {
                Ok(o) => o,
                Err(e) => {
                    if e.0.starts_with("WINDOW_GONE") && attempt == 0 {
                        self.set_window(key, Value::Null, Value::Null);
                        continue;
                    }
                    if let Some(rest) = e.0.strip_prefix("WINDOW_CLOSED") {
                        self.set_window(key, Value::Null, Value::Null);
                        return err(rest.strip_prefix(' ').unwrap_or(rest));
                    }
                    if e.0.starts_with("FRONT_BUSY") {
                        let w = e.0.split_whitespace().last().unwrap_or("").to_string();
                        let who = {
                            let s = self.st.lock();
                            s.users.values().find(|u| s_eq(&u.window, &w)).map(|u| format!("{} ", u.label)).unwrap_or_else(|| "别的 Bot 在对话里用".into())
                        };
                        return err(format!(
                            "屏幕上现在是{who}的窗口，它还在用。要在那个页面上操作就先 cancel_task 停掉它；只是打开网页的话直接 browser_open，会换一个窗口。"
                        ));
                    }
                    return Err(e);
                }
            };
            self.set_window(key, out.get("window").cloned().unwrap_or(Value::Null), out.get("browser").cloned().unwrap_or(Value::Null));
            if py::truthy(&out["window"]) {
                self.st.lock().recent = (out["window"].clone(), out.get("browser").cloned().unwrap_or(Value::Null));
            }
            if self.st.lock().recent_desk {
                self.st.lock().recent_desk = false;
                self.changed();
            }
            return Ok(out);
        }
        err(t!("找不到浏览器窗口"))
    }

    /// 截取 key 自己窗口里的页面（不含浏览器边框），点击坐标以它为准
    pub async fn page_shot(&self, key: &str) -> R<Vec<u8>> {
        let r = self.browser("screenshot", Some(key), false, json!({})).await?;
        if let Some(d) = r["data"].as_str() {
            return Ok(crate::util::b64_decode(d).unwrap_or_default());
        }
        let path = s(&r["path"]);
        let out = self.read_bytes(&path, 32 << 20).await;
        let _ = self.exec(&format!("rm -f {}", quote(&path)), 10, None).await;
        out
    }

    /// 把任务的窗口放到屏幕最前面，并固定看它
    pub async fn front(&self, key: &str) -> R<()> {
        self.browser("front", Some(key), true, json!({})).await?;
        self.st.lock().watch = Some(key.into());
        self.changed();
        Ok(())
    }
}

fn s_eq(v: &Value, w: &str) -> bool {
    py::str_of(v) == w
}

pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

pub fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}
