//! 第一次打开时的准备：测网络（国内就用国内镜像）、起服务、装 uv 和 Python。启动页轮询 setup_state，按这里的步骤画进度。
//!
//! 测网络的办法照 Linger：同时从官方源和国内镜像下载同一个文件的前 512 KB，谁快用谁（国内镜像要快 1.5 倍以上才算国内，
//! 开着代理的也会测成官方源）。都下载不了就不记，下次再测。

use serde::Serialize;
use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Serialize)]
pub struct Step {
    pub id: &'static str,
    /// "pending" / "running" / "done" / "error" / "skipped"
    pub status: &'static str,
    pub detail: String,
    pub done: u64,
    pub total: u64,
    pub secs: f64,
    #[serde(skip)]
    t0: Option<Instant>,
}

#[derive(Default)]
pub struct Onboard {
    steps: Mutex<Vec<Step>>,
}

pub const STEPS: [&str; 4] = ["network", "service", "uv", "python"];

impl Onboard {
    pub fn reset(&self) {
        *self.steps.lock().unwrap() = STEPS
            .iter()
            .map(|id| Step { id, status: "pending", detail: String::new(), done: 0, total: 0, secs: 0.0, t0: None })
            .collect();
    }

    pub fn clear(&self) {
        self.steps.lock().unwrap().clear();
    }

    pub fn snapshot(&self) -> Vec<Step> {
        let mut v = self.steps.lock().unwrap().clone();
        for s in &mut v {
            if let (Some(t), "running") = (s.t0, s.status) {
                s.secs = t.elapsed().as_secs_f64();
            }
        }
        v
    }

    pub fn set(&self, id: &str, status: &'static str, detail: Option<String>) {
        let mut v = self.steps.lock().unwrap();
        if let Some(s) = v.iter_mut().find(|s| s.id == id) {
            if status == "running" && s.status != "running" {
                s.t0 = Some(Instant::now());
                s.done = 0;
                s.total = 0;
            }
            if status != "running" {
                if let Some(t) = s.t0 {
                    s.secs = t.elapsed().as_secs_f64();
                }
            }
            s.status = status;
            if let Some(d) = detail {
                s.detail = d;
            }
        }
    }

    pub fn progress(&self, id: &str, done: u64, total: u64) {
        if let Some(s) = self.steps.lock().unwrap().iter_mut().find(|s| s.id == id) {
            s.done = done;
            s.total = total;
        }
    }

    /// 跑 `osa-server setup`，按它打出的每一行更新 uv、python 两步。全都好了返回 Ok
    pub fn run_setup(&self, mut child: std::process::Child) -> Result<(), String> {
        let out = child.stdout.take().ok_or("no stdout")?;
        let mut failed: Option<String> = None;
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let id = match v["step"].as_str() {
                Some("uv") => "uv",
                Some("python") => "python",
                _ => continue,
            };
            match v["status"].as_str().unwrap_or("") {
                "running" => {
                    self.set(id, "running", None);
                    if let (Some(d), Some(t)) = (v["done"].as_u64(), v["total"].as_u64()) {
                        self.progress(id, d, t);
                    }
                }
                "done" => self.set(id, "done", None),
                "error" => {
                    let m = v["message"].as_str().unwrap_or("").to_string();
                    self.set(id, "error", Some(m.clone()));
                    failed = Some(m);
                }
                _ => {}
            }
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        match failed {
            Some(m) => Err(m),
            None if !status.success() => {
                let msg = format!("osa-server setup: {status}");
                for id in ["uv", "python"] {
                    if self.steps.lock().unwrap().iter().any(|s| s.id == id && s.status != "done") {
                        self.set(id, "error", Some(msg.clone()));
                        break;
                    }
                }
                Err(msg)
            }
            None => Ok(()),
        }
    }
}

/// uv 的安装包在 PyPI 上的路径（官方和阿里云镜像一样），拿来测速：反正下一步就要下载它
const PROBE: &str = "f4/14/77230cc4f6a73a31578cb8070a5f67f58a76946ead643ae8a4155620d6da/uv-0.13.6-py3-none-macosx_11_0_arm64.whl";
const PROBE_BYTES: u64 = 512 * 1024;

fn timed(base: &str) -> Option<Duration> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout(Duration::from_secs(6))
        .user_agent(concat!("open-super-agent-desktop/", env!("CARGO_PKG_VERSION")))
        .build();
    let t = Instant::now();
    let resp = agent.get(&format!("{base}/{PROBE}")).set("Range", &format!("bytes=0-{}", PROBE_BYTES - 1)).call().ok()?;
    let mut buf = Vec::with_capacity(PROBE_BYTES as usize);
    resp.into_reader().take(PROBE_BYTES).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 >= PROBE_BYTES / 2).then(|| t.elapsed())
}

/// 测网络：Some("cn") / Some("global")，都连不上是 None
pub fn detect() -> Option<&'static str> {
    let (tx, rx) = mpsc::channel();
    for (name, base) in [("global", "https://files.pythonhosted.org/packages"), ("cn", "https://mirrors.aliyun.com/pypi/packages")] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((name, timed(base)));
        });
    }
    drop(tx);
    let (mut g, mut c) = (None, None);
    for (name, t) in rx.iter().take(2) {
        if name == "cn" { c = t } else { g = t }
    }
    match (g, c) {
        (Some(g), Some(c)) => Some(if c.as_secs_f64() * 1.5 < g.as_secs_f64() { "cn" } else { "global" }),
        (None, Some(_)) => Some("cn"),
        (Some(_), None) => Some("global"),
        (None, None) => None,
    }
}
