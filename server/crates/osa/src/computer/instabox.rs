//! Instabox 后端：所有 Bot 共用一台 Firecracker 微虚拟机（agent-browser 模板）。对应 osa/computer/instabox.py。
//! API key：界面里填的（保险库）优先，没填用环境变量 INSTABOX_API_KEY。INSTABOX_API_URL 可以换地址（测试用假服务）。

use super::base::{Backend, Computer, ComputerError, ExecResult, R, Vnc, check_enabled, err};
use crate::db::{self, RowExt};
use crate::{py, vault};
use async_trait::async_trait;
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::LazyLock;
use std::time::Duration;

pub const KEY: &str = "INSTABOX_API_KEY";

pub fn api() -> String {
    std::env::var("INSTABOX_API_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "https://api.instabox.sh".into())
}

fn state_of(s: &str) -> &'static str {
    match s {
        "running" => "running",
        "suspended" | "suspending" => "paused",
        "stopped" | "stopping" => "stopped",
        "creating" | "starting" | "resuming" => "starting",
        "deleted" => "none",
        _ => "error",
    }
}

/// API key 从哪来：saved（界面里填的）/ env（环境变量）/ 空（没有）
pub fn key_source() -> &'static str {
    if vault::get(KEY).is_some_and(|v| !v.is_empty()) {
        "saved"
    } else if std::env::var(KEY).is_ok_and(|v| !v.is_empty()) {
        "env"
    } else {
        ""
    }
}

fn key() -> R<String> {
    vault::get(KEY).filter(|v| !v.is_empty()).or_else(|| std::env::var(KEY).ok().filter(|v| !v.is_empty())).ok_or_else(|| ComputerError(t!("还没填 Instabox 的 API Key")))
}

static HTTP: LazyLock<reqwest::Client> =
    LazyLock::new(|| reqwest::Client::builder().connect_timeout(Duration::from_secs(60)).timeout(Duration::from_secs(660)).build().expect("http"));

/// 界面里填 API key：先试一下能不能列出机器，能用才存。换了 key 就忘掉之前记下的机器；那个账号里有以前给这里建的机器就接着用
pub async fn save_key(key: &str) -> R<()> {
    let key = key.trim();
    let mut ours: Option<String> = None;
    if !key.is_empty() {
        let r = HTTP
            .get(format!("{}/v1/machines", api()))
            .header("X-API-Key", key)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| ComputerError(t!("连不上 Instabox：{error}", error = e.to_string())))?;
        let code = r.status().as_u16();
        if code == 401 || code == 403 {
            return err(t!("Instabox 不认这个 API Key（{code}），检查一下有没有复制完整、权限里有没有 machines:write", code = code));
        }
        let text = r.text().await.unwrap_or_default();
        if code >= 400 {
            return err(format!("Instabox {code} {}", py::head(&text, 200)));
        }
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            ours = v["machines"]
                .as_array()
                .and_then(|a| {
                    a.iter().find(|m| !["deleting", "deleted"].contains(&m["state"].as_str().unwrap_or("")) && m["metadata"]["app"] == "open-super-agent")
                })
                .and_then(|m| m["id"].as_str().map(String::from));
        }
    }
    if key == vault::get(KEY).unwrap_or_default() {
        return Ok(());
    }
    if key.is_empty() {
        vault::delete(KEY);
    } else {
        vault::put(KEY, key).map_err(ComputerError)?;
    }
    db::exec("delete from computers where backend='instabox'", []);
    if let Some(o) = ours {
        db::exec("insert or replace into computers(bot_id, backend, ref) values(0, 'instabox', ?)", params![o]);
    }
    Ok(())
}

pub struct Instabox {
    pub bot_id: i64,
    pub fixed: Option<String>,
    pub template: String,
    pub idle_seconds: i64,
    starting: tokio::sync::Mutex<()>,
}

impl Instabox {
    pub fn new(bot_id: i64, template: &str, idle_seconds: i64, fixed: Option<String>) -> Instabox {
        Instabox { bot_id, fixed, template: template.into(), idle_seconds, starting: tokio::sync::Mutex::new(()) }
    }

    pub fn machine_id(&self) -> Option<String> {
        if let Some(f) = &self.fixed {
            return Some(f.clone());
        }
        db::row("select ref from computers where bot_id=? and backend='instabox'", params![self.bot_id]).map(|r| r.s(0))
    }

    fn save_id(&self, mid: &str) {
        db::exec("insert or replace into computers(bot_id, backend, ref) values(?, 'instabox', ?)", params![self.bot_id, mid]);
    }

    async fn req(&self, method: reqwest::Method, path: &str, body: Option<Value>, idem: bool, timeout: Option<f64>) -> R<Value> {
        let mut rb = HTTP.request(method, format!("{}{path}", api())).header("X-API-Key", key()?);
        if idem {
            rb = rb.header("Idempotency-Key", uuid4());
        }
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        if let Some(t) = timeout {
            rb = rb.timeout(Duration::from_secs_f64(t));
        }
        let r = rb.send().await.map_err(|e| ComputerError(e.to_string()))?;
        let code = r.status().as_u16();
        let text = r.text().await.unwrap_or_default();
        if code >= 400 {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .map(|v| {
                    let e = &v["error"];
                    format!("{}: {}", py::str_of(&e["code"]), py::str_of(&e["message"]))
                })
                .unwrap_or_else(|| py::head(&text, 300));
            return err(format!("Instabox {code} {msg}"));
        }
        Ok(if text.is_empty() { json!({}) } else { serde_json::from_str(&text).unwrap_or(json!({})) })
    }

    async fn wait_op(&self, mut op: Value) -> R<Value> {
        while !["succeeded", "failed"].contains(&op["state"].as_str().unwrap_or("")) {
            op = self.req(reqwest::Method::GET, &format!("/v1/operations/{}?wait=25s", py::str_of(&op["id"])), None, false, Some(40.0)).await?;
        }
        if op["state"] == "failed" {
            return err(t!("Instabox 操作失败：{error}", error = op["error"].clone()));
        }
        Ok(op)
    }

    async fn desktop(&self, mid: &str, body: Value, timeout: Option<f64>) -> R<Value> {
        self.req(reqwest::Method::POST, &format!("/v1/machines/{mid}/desktop"), Some(body), false, timeout).await
    }
}

pub fn uuid4() -> String {
    let b: [u8; 16] = rand::random();
    let mut b = b;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[async_trait]
impl Backend for Instabox {
    fn kind(&self) -> &'static str {
        "instabox"
    }

    fn fixed_machine(&self) -> bool {
        self.fixed.is_some()
    }

    async fn status(&self, _c: &Computer) -> Value {
        let Some(mid) = self.machine_id() else {
            return json!({"state": "none", "detail": t!("还没有创建")});
        };
        match self.req(reqwest::Method::GET, &format!("/v1/machines/{mid}"), None, false, None).await {
            Err(e) => json!({"state": "error", "detail": e.0}),
            Ok(m) => {
                let s = m["state"].as_str().unwrap_or("");
                json!({"state": state_of(s), "detail": s, "machine_id": mid})
            }
        }
    }

    async fn start(&self, c: &Computer) -> R<()> {
        check_enabled()?;
        let _l = self.starting.lock().await;
        let st = self.status(c).await;
        let mut mid = self.machine_id();
        if st["state"] == "none" || mid.is_none() {
            let op = match self
                .req(
                    reqwest::Method::POST,
                    "/v1/machines",
                    Some(json!({"name": "osa-computer", "templateId": self.template, "autoSuspendAfterSeconds": self.idle_seconds,
                                "metadata": {"app": "open-super-agent", "role": "shared"}})),
                    true,
                    None,
                )
                .await
            {
                Ok(op) => op,
                Err(e) if e.0.contains(" 429 ") => {
                    return err(t!("Instabox 账号的机器数量到上限了（免费版只能有一台）。可以在电脑面板里选“用已有的电脑”，或者去 Instabox 删掉一台。"));
                }
                Err(e) => return Err(e),
            };
            let op = self.wait_op(op).await?;
            let m = py::str_of(&op["machineId"]);
            self.save_id(&m);
            mid = Some(m);
        } else if st["detail"] == "suspended" {
            let m = mid.clone().unwrap_or_default();
            let op = self.req(reqwest::Method::POST, &format!("/v1/machines/{m}/resume"), None, true, None).await?;
            self.wait_op(op).await?;
        } else if st["detail"] == "stopped" {
            let m = mid.clone().unwrap_or_default();
            let op = self.req(reqwest::Method::POST, &format!("/v1/machines/{m}/start"), None, true, None).await?;
            self.wait_op(op).await?;
        } else if st["state"] == "running" {
            let m = mid.clone().unwrap_or_default();
            self.req(reqwest::Method::POST, &format!("/v1/machines/{m}/wake"), Some(json!({})), false, None).await?;
        }
        let mid = mid.unwrap_or_default();
        let (w, h) = {
            let s = c.st.lock();
            (s.width, s.height)
        };
        let mut d = self.desktop(&mid, json!({"action": "start", "width": w, "height": h}), Some(100.0)).await?;
        if !py::truthy(&d["installed"]) {
            if !py::truthy(&d["installing"]) {
                self.desktop(&mid, json!({"action": "install"}), None).await?;
            }
            for _ in 0..120 {
                tokio::time::sleep(Duration::from_secs(3)).await;
                d = self.desktop(&mid, json!({"action": "status"}), None).await?;
                if py::truthy(&d["failed"]) {
                    return err(t!("桌面安装失败：{error}", error = py::head(&py::str_of(&d.get("log").cloned().unwrap_or(json!(""))), 300)));
                }
                if py::truthy(&d["installed"]) {
                    break;
                }
            }
            self.desktop(&mid, json!({"action": "start", "width": w, "height": h}), Some(100.0)).await?;
        }
        let r = self.exec(c, "mkdir -p /data/workspace; DISPLAY=:1 xdpyinfo 2>/dev/null | awk '/dimensions:/{print $2}'", 20, None).await?;
        if let Some((a, b)) = r.stdout.trim().split_once('x') {
            if let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) {
                let mut s = c.st.lock();
                s.width = a;
                s.height = b;
            }
        }
        Ok(())
    }

    async fn pause(&self, c: &Computer) -> R<()> {
        if let Some(mid) = self.machine_id() {
            if self.status(c).await["state"] == "running" {
                let op = self.req(reqwest::Method::POST, &format!("/v1/machines/{mid}/suspend"), None, true, None).await?;
                self.wait_op(op).await?;
            }
        }
        Ok(())
    }

    async fn exec(&self, _c: &Computer, cmd: &str, timeout: i64, stdin: Option<&str>) -> R<ExecResult> {
        let Some(mid) = self.machine_id() else {
            return err(t!("电脑还没创建"));
        };
        let mut body = json!({"cmd": cmd, "timeoutSeconds": timeout.clamp(1, 600), "cwd": "/data/workspace"});
        if let Some(s) = stdin {
            body["stdin"] = json!(s);
        }
        let r = self.req(reqwest::Method::POST, &format!("/v1/machines/{mid}/exec"), Some(body), false, Some((timeout + 30) as f64)).await?;
        Ok(ExecResult::new(
            py::to_int(&r["exitCode"]).unwrap_or(-1),
            r["stdout"].as_str().unwrap_or(""),
            r["stderr"].as_str().unwrap_or(""),
            py::truthy(&r["timedOut"]),
        ))
    }

    async fn read_bytes(&self, c: &Computer, path: &str, limit: usize) -> R<Vec<u8>> {
        let mid = self.machine_id().unwrap_or_default();
        let r = HTTP
            .get(format!("{}/v1/machines/{mid}/files/content", api()))
            .query(&[("path", path)])
            .header("X-API-Key", key()?)
            .send()
            .await
            .map_err(|e| ComputerError(e.to_string()))?;
        let code = r.status().as_u16();
        if code == 404 {
            return err(t!("文件不存在：{path}", path = path));
        }
        if code >= 400 {
            return c.read_bytes_sh(path, limit).await;
        }
        let b = r.bytes().await.map_err(|e| ComputerError(e.to_string()))?;
        if b.len() > limit {
            return err(t!("文件太大"));
        }
        Ok(b.to_vec())
    }

    fn on_takeover(&self, _c: &Computer) {
        self.tell_desktop("takeover");
    }

    fn on_handback(&self, _c: &Computer) {
        self.tell_desktop("handback");
    }

    async fn open_vnc(&self, _c: &Computer) -> R<Vnc> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mid = self.machine_id().unwrap_or_default();
        let url = format!("{}/v1/machines/{mid}/vnc", api().replace("https://", "wss://").replace("http://", "ws://"));
        let mut req = url.into_client_request().map_err(|e| ComputerError(e.to_string()))?;
        req.headers_mut().insert("X-API-Key", key()?.parse().map_err(|_| ComputerError("bad key".into()))?);
        let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| ComputerError(e.to_string()))?;
        Ok(Vnc::Ws(Box::new(ws)))
    }

    async fn list_machines(&self, _c: &Computer) -> Option<R<Vec<Value>>> {
        Some(async {
            let mut out: Vec<Value> = vec![];
            let mut cursor: Option<String> = None;
            for _ in 0..10 {
                let path = match &cursor {
                    Some(c) => format!("/v1/machines?cursor={c}"),
                    None => "/v1/machines".into(),
                };
                let d = self.req(reqwest::Method::GET, &path, None, false, None).await?;
                out.extend(d["machines"].as_array().cloned().unwrap_or_default());
                cursor = d["nextCursor"].as_str().filter(|s| !s.is_empty()).map(String::from);
                if cursor.is_none() {
                    break;
                }
            }
            let cur = self.machine_id();
            Ok(out
                .into_iter()
                .filter(|m| !["deleting", "deleted"].contains(&m["state"].as_str().unwrap_or("")))
                .map(|m| {
                    json!({"id": m["id"], "name": m["name"].as_str().unwrap_or(""), "state": state_of(m["state"].as_str().unwrap_or("")),
                           "template": m["template"].as_str().unwrap_or(""), "created": m.get("createdAt").cloned().unwrap_or(Value::Null),
                           "ours": m["metadata"]["app"] == "open-super-agent", "current": m["id"].as_str() == cur.as_deref()})
                })
                .collect())
        }
        .await)
    }

    fn use_machine(&self, _c: &Computer, mid: &str) -> Option<R<()>> {
        if self.fixed.is_some() {
            return Some(err(t!("config.yaml 的 computer.instabox_machine 已经指定了机器，要换请先清空那一项")));
        }
        self.save_id(mid);
        Some(Ok(()))
    }
}

impl Instabox {
    fn tell_desktop(&self, action: &str) {
        let Some(mid) = self.machine_id() else { return };
        let Ok(k) = key() else { return };
        let action = action.to_string();
        tokio::spawn(async move {
            let _ = HTTP.post(format!("{}/v1/machines/{mid}/desktop", api())).header("X-API-Key", k).json(&json!({"action": action})).send().await;
        });
    }
}
