//! 用哪种电脑、能选哪几种、这台机器上能不能用（对应 osa/computer/__init__.py 的 backend_name / choices / available / docker_problem，
//! 和 host.py 的 chrome_path）。
use crate::{config, i18n, security};
use serde_json::{Value, json};

pub const BACKENDS: [&str; 3] = ["host", "local", "instabox"];

pub fn host_allowed() -> bool {
    (std::env::var("OSA_DESKTOP").is_ok_and(|v| !v.is_empty()) || cfg!(target_os = "macos")) && security::local_exec()
}

pub fn key_source() -> &'static str {
    super::instabox::key_source()
}


pub fn backend_name() -> String {
    let mut b = config::get("computer.backend").as_str().unwrap_or("auto").to_string();
    if b == "auto" && std::env::var("OSA_DESKTOP").is_ok_and(|v| !v.is_empty()) {
        b = "host".into();
    }
    if !BACKENDS.contains(&b.as_str()) || (b == "host" && !host_allowed()) {
        return if key_source().is_empty() { "local".into() } else { "instabox".into() };
    }
    b
}

pub fn choices() -> Vec<&'static str> {
    let want: Vec<String> =
        config::get("computer.choices").as_array().map(|a| a.iter().map(crate::py::str_of).collect()).unwrap_or_default();
    BACKENDS.iter().copied().filter(|b| (want.is_empty() || want.iter().any(|w| w == b)) && (*b != "host" || host_allowed())).collect()
}

fn exists(p: &str) -> bool {
    match std::fs::metadata(p) {
        Ok(m) => m.is_file(),
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
    }
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file()).map(|p| p.display().to_string())
}

pub fn chrome_path() -> Option<String> {
    if let Some(f) = config::get("computer.chrome_path").as_str().filter(|s| !s.is_empty()) {
        let f = config::expand_user(std::path::Path::new(f)).display().to_string();
        return if exists(&f) { Some(f) } else { None };
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let list: Vec<String> = if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
            format!("{home}/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into(),
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".into(),
        ]
    } else {
        vec![]
    };
    if let Some(c) = list.into_iter().find(|c| exists(c)) {
        return Some(c);
    }
    ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge"].iter().find_map(|n| which(n))
}

pub fn docker_problem() -> String {
    let socket = std::env::var("OSA_COMPUTER_SOCKET").unwrap_or_default();
    if !socket.is_empty() {
        #[cfg(unix)]
        {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return String::new();
            }
        }
        return i18n::t0("电脑容器没开（docker compose up -d computer）");
    }
    if which("docker").is_some() { String::new() } else { i18n::t0("这台电脑没装 Docker") }
}

pub fn available() -> Vec<Value> {
    let host = if host_allowed() {
        json!({"id": "host", "ok": chrome_path().is_some()})
    } else {
        json!({"id": "host", "ok": false, "locked": true})
    };
    let key = key_source();
    let why = docker_problem();
    vec![host, json!({"id": "local", "ok": why.is_empty(), "why": why}), json!({"id": "instabox", "ok": !key.is_empty(), "key": key})]
}

pub fn docker_ok() -> bool {
    docker_problem().is_empty()
}
