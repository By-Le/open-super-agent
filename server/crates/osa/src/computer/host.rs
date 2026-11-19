//! 本机电脑：Bot 用这台 Mac 上装的 Chrome（osa/computer/host.py）。第 2 步 B 再迁：现在只有占位，选了它会照实说还不能用。
//!
//! 迁的时候实现 `Backend` 里 HOSTS 那一组（shell_enabled、desktop_enabled、desk_*、show、grant、peek）和文件的那几个
//! （read_bytes / read_range / write_bytes / ls / stat_paths / rm / upload_paths 直接读写工作文件夹），helper_run 在本机跑 cdp_helper.py。

use super::base::{Backend, Computer, ExecResult, R, Vnc, err};
use async_trait::async_trait;
use serde_json::{Value, json};

pub use super::pc::chrome_path;

pub fn not_yet() -> String {
    "Rust 版还没有本机电脑（第 2 步 B），先在设置里选 Docker 或 Instabox".into()
}

pub struct Host {
    pub root: String,
}

impl Host {
    pub fn new() -> Host {
        let root = crate::config::DATA.join("workspace");
        let _ = std::fs::create_dir_all(&root);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Host { root: root.display().to_string() }
    }
}

#[async_trait]
impl Backend for Host {
    fn kind(&self) -> &'static str {
        "host"
    }
    fn workspace(&self) -> String {
        self.root.clone()
    }
    fn auto_pause(&self) -> bool {
        false
    }
    async fn status(&self, _c: &Computer) -> Value {
        json!({"state": "error", "detail": not_yet()})
    }
    async fn start(&self, _c: &Computer) -> R<()> {
        err(not_yet())
    }
    async fn pause(&self, _c: &Computer) -> R<()> {
        Ok(())
    }
    async fn exec(&self, _c: &Computer, _cmd: &str, _timeout: i64, _stdin: Option<&str>) -> R<ExecResult> {
        err(not_yet())
    }
    async fn open_vnc(&self, _c: &Computer) -> R<Vnc> {
        err(not_yet())
    }
}

pub fn shell_on() -> bool {
    crate::config::get_bool("computer.host_shell", false) && crate::security::local_exec()
}
