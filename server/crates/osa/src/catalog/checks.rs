//! 连接前的检查：添加应用前先试一下用户填的东西对不对。对应 osa/catalog/checks.py。

use crate::py;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub const NAMES: [&str; 1] = ["imap"];

/// 检查的参数：(必须写的, 能写的)
pub fn signature(name: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match name {
        "imap" => Some((&["host", "user", "password"], &["host", "user", "password", "port", "hint"])),
        _ => None,
    }
}

pub async fn run(name: &str, params: &Value) -> Result<(), String> {
    let s = |k: &str| params[k].as_str().map(String::from);
    match name {
        "imap" => imap(&s("host").unwrap_or_default(), &s("user").unwrap_or_default(), &s("password").unwrap_or_default(), &s("port").unwrap_or_else(|| "993".into()), &s("hint").unwrap_or_default()).await,
        _ => Ok(()),
    }
}

/// IMAP 的字符串参数（imaplib 的 _quote）
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 用 IMAP（SSL）登录一次。hint 接在登录失败的提示后面
pub async fn imap(host: &str, user: &str, password: &str, port: &str, hint: &str) -> Result<(), String> {
    let cant = |e: String| t!("连不上邮件服务器 {host}：{error}", host = host, error = e);
    let port: u16 = port.trim().parse().map_err(|_| cant(format!("invalid literal for int() with base 10: '{port}'")))?;
    use rustls_platform_verifier::ConfigVerifierExt;
    let cfg = rustls::ClientConfig::with_platform_verifier().map_err(|e| cant(e.to_string()))?;
    let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| cant(e.to_string()))?;
    let fut = async {
        let tcp = tokio::net::TcpStream::connect((host, port)).await.map_err(|e| cant(e.to_string()))?;
        let tls = conn.connect(name, tcp).await.map_err(|e| cant(e.to_string()))?;
        let (r, mut w) = tokio::io::split(tls);
        let mut lines = BufReader::new(r).lines();
        let greet = lines.next_line().await.map_err(|e| cant(e.to_string()))?.unwrap_or_default();
        if !greet.starts_with("* OK") && !greet.starts_with("* PREAUTH") {
            return Err(cant(greet));
        }
        w.write_all(format!("a1 LOGIN {} {}\r\n", quote(user), quote(password)).as_bytes()).await.map_err(|e| cant(e.to_string()))?;
        let mut result = Err(cant("connection closed".into()));
        while let Ok(Some(l)) = lines.next_line().await {
            if let Some(rest) = l.strip_prefix("a1 ") {
                result = if rest.starts_with("OK") {
                    Ok(())
                } else {
                    let detail = rest.split_once(' ').map(|x| x.1).unwrap_or(rest);
                    let msg = t!("邮箱登录失败：{detail}", detail = py::head(detail, 120));
                    Err(if hint.is_empty() { msg } else { t!("{msg}。{hint}", msg = msg, hint = hint) })
                };
                break;
            }
        }
        let _ = w.write_all(b"a2 LOGOUT\r\n").await;
        result
    };
    tokio::time::timeout(Duration::from_secs(20), fut).await.map_err(|_| cant("timed out".into()))?
}
