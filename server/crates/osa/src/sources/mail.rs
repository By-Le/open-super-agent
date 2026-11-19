//! 收到新邮件（IMAP）：只看不动（EXAMINE、BODY.PEEK），看到哪了记 UIDVALIDITY 和最大的 UID。对应 osa/sources/mail.py（说明见那里）。
//!
//! Python 版用 imaplib（同步，放在线程里）；这里是一个只做这几件事的异步 IMAP 客户端：LOGIN、ID、EXAMINE、FETCH、UID SEARCH、UID FETCH。

use super::util::{oneline, short, text_of};
use crate::registry::Source;
use crate::{config, py, tools};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const PROVIDERS: [(&str, &str, u16); 9] = [
    ("qq", "imap.qq.com", 993), ("163", "imap.163.com", 993), ("126", "imap.126.com", 993), ("gmail", "imap.gmail.com", 993),
    ("outlook", "outlook.office365.com", 993), ("icloud", "imap.mail.me.com", 993), ("aliyun", "imap.aliyun.com", 993),
    ("139", "imap.139.com", 993), ("feishu", "imap.feishu.cn", 993),
];
const DOMAINS: [(&str, &str); 17] = [
    ("qq.com", "qq"), ("foxmail.com", "qq"), ("vip.qq.com", "qq"), ("163.com", "163"), ("vip.163.com", "163"), ("126.com", "126"),
    ("gmail.com", "gmail"), ("googlemail.com", "gmail"), ("outlook.com", "outlook"), ("hotmail.com", "outlook"), ("live.com", "outlook"),
    ("msn.com", "outlook"), ("icloud.com", "icloud"), ("me.com", "icloud"), ("mac.com", "icloud"), ("aliyun.com", "aliyun"), ("139.com", "139"),
];
fn hint(p: &str) -> Option<&'static str> {
    match p {
        "gmail" => Some("Gmail 要先开两步验证，再生成“应用专用密码”"),
        "icloud" => Some("iCloud 要在 Apple 账户里生成 App 专用密码"),
        "outlook" => Some("Outlook / Hotmail 的个人邮箱现在大多只能用 OAuth 登录，授权码可能登不上"),
        _ => None,
    }
}
const NEED_ID: [&str; 4] = ["163.com", "126.com", "yeah.net", "188.com"];
const LOGIN_FAILED: &str = "登录失败：检查邮箱和授权码；QQ / 163 邮箱要先在网页版设置里开启 IMAP、生成授权码";
const UNSAFE: &str = "163 / 126 邮箱拒绝了这次登录（Unsafe Login）：到网页版设置里开启 IMAP 服务、用授权码登录，再试一次";
const TIMEOUT: u64 = 20;
const MAX_NEW: usize = 20;
const MAX_FETCH: usize = 200_000;
const SNIPPET: usize = 1000;

// ---------- 服务器 ----------
pub fn guess(user: &str) -> String {
    let d = user.rsplit('@').next().unwrap_or("").trim().to_lowercase();
    DOMAINS.iter().find(|(k, _)| *k == d).map(|(_, v)| v.to_string()).unwrap_or_default()
}

fn host_port(host: &str, port: &Value) -> (String, u16) {
    static PFX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(imaps?|https?)://").unwrap());
    static HP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^:\[\]]+):(\d+)$").unwrap());
    let h = PFX.replace(host.trim(), "").split('/').next().unwrap_or("").to_string();
    let mut port = if py::truthy(port) { py::to_int(port).unwrap_or(993) as u16 } else { 993 };
    let mut h = h;
    if let Some(m) = HP.captures(&h.clone()) {
        h = m[1].to_string();
        port = m[2].parse().unwrap_or(993);
    }
    (h.to_lowercase(), port)
}

pub fn server(p: &Value) -> Result<(String, u16), String> {
    let mut prov = if py::truthy(&p["provider"]) { py::str_of(&p["provider"]) } else { "auto".into() };
    if prov == "auto" {
        prov = guess(p["user"].as_str().unwrap_or(""));
    }
    if let Some((_, h, port)) = PROVIDERS.iter().find(|(k, _, _)| *k == prov) {
        return Ok((h.to_string(), *port));
    }
    let (host, port) = host_port(p["host"].as_str().unwrap_or(""), &p["port"]);
    if host.is_empty() {
        return Err("选了“其他”要填 IMAP 服务器".into());
    }
    Ok((host, port))
}

// ---------- 解码 ----------
fn bytes_to(b: &[u8], cs: Option<&str>) -> String {
    let cs = cs.unwrap_or("utf-8").to_lowercase();
    let cs = if ["gb2312", "gbk", "gb_2312-80", "x-gbk", "cp936", "euc-cn"].contains(&cs.as_str()) { "gb18030".to_string() } else { cs };
    match encoding_rs::Encoding::for_label(cs.as_bytes()) {
        Some(e) => e.decode_without_bom_handling(b).0.into_owned(),
        None => String::from_utf8_lossy(b).into_owned(),
    }
}

/// 头里直接写的 8 位字节：先按 UTF-8，不行按 GB18030
fn raw_header(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::GB18030.decode_without_bom_handling(b).0.into_owned(),
    }
}

static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// 邮件头 → 文字：=?UTF-8?B?…?= 这类编码解开，gb2312 按 GB18030 解
pub fn decode(s: &str) -> String {
    static EW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)=\?([^?]+)\?([bq])\?([^?]*)\?=").unwrap());
    static GAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\?=)\s+(=\?)").unwrap());
    // 相邻的编码词之间的空白不算（RFC 2047）
    let joined = GAP.replace_all(s, "$1$2").into_owned();
    let out = EW
        .replace_all(&joined, |c: &regex::Captures| {
            let cs = c[1].split('*').next().unwrap_or("utf-8");
            let data = &c[3];
            let raw = if c[2].eq_ignore_ascii_case("b") {
                crate::util::b64_decode(data).or_else(|| crate::util::b64_decode(&format!("{data}{}", "=".repeat((4 - data.len() % 4) % 4)))).unwrap_or_default()
            } else {
                qp_word(data)
            };
            bytes_to(&raw, Some(cs))
        })
        .into_owned();
    WS.replace_all(&out, " ").trim().to_string()
}

fn hex2(b: &[u8], i: usize) -> Option<u8> {
    let h = b.get(i + 1..i + 3)?;
    u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()
}

/// 编码词里的 Q 编码：_ 是空格，=XX 是一个字节
fn qp_word(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => out.push(b' '),
            b'=' => {
                if let Some(v) = hex2(b, i) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'=');
            }
            c => out.push(c),
        }
        i += 1;
    }
    out
}

fn attachments_in(bs: &[u8]) -> Vec<String> {
    static R: LazyLock<regex::bytes::Regex> = LazyLock::new(|| regex::bytes::Regex::new(r#"(?i)"(?:FILE)?NAME(\*)?"\s+"((?:[^"\\]|\\.)*)""#).unwrap());
    let mut out: Vec<String> = vec![];
    for m in R.captures_iter(bs) {
        let mut v = String::from_utf8_lossy(&m[2]).replace("\\\"", "\"");
        if m.get(1).is_some() {
            let (cs, rest) = v.split_once("''").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((v.clone(), String::new()));
            let (enc, data) = if rest.is_empty() { ("utf-8".to_string(), cs) } else { (if cs.is_empty() { "utf-8".into() } else { cs }, rest) };
            let raw: Vec<u8> = percent(&data);
            v = bytes_to(&raw, Some(&enc));
        } else {
            v = decode(&v);
        }
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

fn percent(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = hex2(b, i) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// "名字 <地址>" → (名字, 地址)，和 email.utils.parseaddr 差不多
fn parseaddr(s: &str) -> (String, String) {
    let list = getaddresses(s);
    list.into_iter().next().unwrap_or_default()
}

fn getaddresses(s: &str) -> Vec<(String, String)> {
    match mailparse::addrparse(s) {
        Ok(list) => {
            let mut out = vec![];
            for a in list.iter() {
                match a {
                    mailparse::MailAddr::Single(i) => out.push((i.display_name.clone().unwrap_or_default(), i.addr.clone())),
                    mailparse::MailAddr::Group(g) => {
                        for i in &g.addrs {
                            out.push((i.display_name.clone().unwrap_or_default(), i.addr.clone()));
                        }
                    }
                }
            }
            out
        }
        Err(_) => {
            static ANGLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(.*?)\s*<([^>]*)>\s*$").unwrap());
            s.split(',')
                .filter(|x| !x.trim().is_empty())
                .map(|x| match ANGLE.captures(x) {
                    Some(m) => (m[1].trim_matches('"').to_string(), m[2].to_string()),
                    None => (String::new(), x.trim().to_string()),
                })
                .collect()
        }
    }
}

/// 一封信（可能只有开头）→ 事件
pub fn message(raw: &[u8], size: usize, names: &[String], key: &str) -> Result<Value, String> {
    let msg = mailparse::parse_mail(raw).map_err(|e| e.to_string())?;
    let h = |k: &str| -> String {
        msg.headers.iter().find(|x| x.get_key_ref().eq_ignore_ascii_case(k)).map(|x| decode(&raw_header(x.get_value_raw()))).unwrap_or_default()
    };
    let (name, addr) = parseaddr(&h("From"));
    let subject = {
        let s = h("Subject");
        if s.is_empty() { "（无主题）".to_string() } else { s }
    };
    let (mut plain, mut html, mut files) = (String::new(), String::new(), vec![]);
    fn walk<'a>(p: &'a mailparse::ParsedMail<'a>, out: &mut Vec<&'a mailparse::ParsedMail<'a>>) {
        if p.subparts.is_empty() {
            out.push(p);
        } else {
            for s in &p.subparts {
                walk(s, out);
            }
        }
    }
    let mut leaves = vec![];
    walk(&msg, &mut leaves);
    for part in leaves {
        if part.ctype.mimetype.to_lowercase().starts_with("multipart/") {
            continue;
        }
        let cd = part.get_content_disposition();
        let disp_hdr = part.headers.iter().find(|x| x.get_key_ref().eq_ignore_ascii_case("Content-Disposition")).map(|x| x.get_value()).unwrap_or_default();
        let disp = disp_hdr.split(';').next().unwrap_or("").trim().to_lowercase();
        let fname = cd.params.get("filename").cloned().or_else(|| part.ctype.params.get("name").cloned());
        if fname.is_some() || disp == "attachment" {
            let has_cid = part.headers.iter().any(|x| x.get_key_ref().eq_ignore_ascii_case("Content-ID"));
            if let Some(f) = &fname {
                if !(disp != "attachment" && has_cid) {
                    files.push(decode(f));
                }
            }
            continue;
        }
        let ct = part.ctype.mimetype.to_lowercase();
        let charset = part.ctype.params.get("charset").map(|s| s.to_lowercase());
        let body = || bytes_to(&part.get_body_raw().unwrap_or_default(), charset.as_deref());
        if ct == "text/plain" && plain.trim().is_empty() {
            plain = body();
        } else if ct == "text/html" && html.is_empty() {
            html = body();
        }
    }
    static BLANK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n+").unwrap());
    let body = if !plain.trim().is_empty() { plain } else if !html.is_empty() { text_of(&html) } else { String::new() };
    let body = BLANK.replace_all(&body, "\n\n").trim().to_string();
    for n in names {
        if !files.contains(n) {
            files.push(n.clone());
        }
    }
    let date = h("Date");
    let when = match mailparse::dateparse(&date) {
        Ok(ts) if !date.is_empty() => py::strftime_at(ts as f64, "%Y-%m-%d %H:%M"),
        _ => date,
    };
    let fmt = |v: Vec<(String, String)>| -> Vec<String> {
        v.into_iter().filter(|(_, a)| !a.is_empty()).map(|(n, a)| if n.is_empty() { a } else { format!("{n} <{a}>") }).take(5).collect()
    };
    let to = fmt(getaddresses(&h("To")));
    let cc = fmt(getaddresses(&h("Cc")));
    let sender = if !name.is_empty() && !addr.is_empty() { format!("{name} <{addr}>") } else if !name.is_empty() { name.clone() } else { addr.clone() };
    let lines = [
        if sender.is_empty() { String::new() } else { format!("发件人：{sender}") },
        if to.is_empty() { String::new() } else { format!("收件人：{}", to.join("、")) },
        if cc.is_empty() { String::new() } else { format!("抄送：{}", cc.join("、")) },
        if when.is_empty() { String::new() } else { format!("时间：{when}") },
        if files.is_empty() { String::new() } else { format!("附件：{}", files.iter().take(20).cloned().collect::<Vec<_>>().join("、")) },
        if size > 0 && size > raw.len() && body.is_empty() { "（信比较大，只看了开头）".into() } else { String::new() },
        if body.is_empty() { String::new() } else { format!("\n正文：\n{}", short(&body, SNIPPET)) },
    ];
    let who = if !name.is_empty() { name } else if !addr.is_empty() { addr } else { "（不知道是谁）".into() };
    Ok(json!({"id": if key.is_empty() { h("Message-ID") } else { key.to_string() },
              "title": format!("{who} 来信：{}", oneline(&subject, 200)),
              "text": lines.iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join("\n")}))
}

/// 文件夹名 → IMAP 用的 modified UTF-7（“已发送” → “&XfJT0ZAB-”）
pub fn utf7(s: &str) -> String {
    let mut out = String::new();
    let mut buf: Vec<char> = vec![];
    let flush = |buf: &mut Vec<char>, out: &mut String| {
        if !buf.is_empty() {
            let units: Vec<u8> = buf.iter().collect::<String>().encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
            let b64 = crate::util::b64(&units).trim_end_matches('=').replace('/', ",");
            out.push_str(&format!("&{b64}-"));
            buf.clear();
        }
    };
    for ch in s.chars() {
        if (0x20..=0x7E).contains(&(ch as u32)) {
            flush(&mut buf, &mut out);
            if ch == '&' {
                out.push_str("&-");
            } else {
                out.push(ch);
            }
        } else {
            buf.push(ch);
        }
    }
    flush(&mut buf, &mut out);
    out
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---------- IMAP ----------
trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// 一条回应：一行（中间的字面量 {n} 已经读进 literals）
struct Line {
    text: Vec<u8>,
    literals: Vec<Vec<u8>>,
}

struct Imap {
    io: BufReader<Box<dyn Stream>>,
    tag: u32,
    caps: Vec<String>,
}

#[derive(Debug)]
enum ImapErr {
    /// 断了
    Abort(String),
    Io(String),
}

impl Imap {
    async fn line(&mut self) -> Result<Line, ImapErr> {
        static LIT: LazyLock<regex::bytes::Regex> = LazyLock::new(|| regex::bytes::Regex::new(r"\{(\d+)\}\r?\n?$").unwrap());
        let mut text = vec![];
        let mut literals = vec![];
        loop {
            let mut part = vec![];
            let n = self.io.read_until(b'\n', &mut part).await.map_err(|e| ImapErr::Io(e.to_string()))?;
            if n == 0 {
                return Err(ImapErr::Abort("socket error: EOF".into()));
            }
            let lit = LIT.captures(&part).and_then(|c| std::str::from_utf8(&c[1]).ok()?.parse::<usize>().ok());
            while part.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                part.pop();
            }
            text.extend_from_slice(&part);
            match lit {
                Some(len) => {
                    let mut buf = vec![0u8; len];
                    self.io.read_exact(&mut buf).await.map_err(|e| ImapErr::Io(e.to_string()))?;
                    literals.push(buf);
                }
                None => return Ok(Line { text, literals }),
            }
        }
    }

    /// 发一条命令，读到它的结束行。返回 (状态, 结束行里状态后面的文字, 中间的回应)
    async fn cmd(&mut self, c: &str) -> Result<(String, String, Vec<Line>), ImapErr> {
        self.tag += 1;
        let tag = format!("A{:03}", self.tag);
        self.io.get_mut().write_all(format!("{tag} {c}\r\n").as_bytes()).await.map_err(|e| ImapErr::Io(e.to_string()))?;
        self.io.get_mut().flush().await.map_err(|e| ImapErr::Io(e.to_string()))?;
        let mut got = vec![];
        loop {
            let l = self.line().await?;
            let s = String::from_utf8_lossy(&l.text).into_owned();
            if let Some(rest) = s.strip_prefix(&format!("{tag} ")) {
                let (status, why) = rest.split_once(' ').unwrap_or((rest, ""));
                return Ok((status.to_uppercase(), why.to_string(), got));
            }
            if s.starts_with("* BYE") && !c.starts_with("LOGOUT") {
                return Err(ImapErr::Abort(s[2..].trim().to_string()));
            }
            got.push(l);
        }
    }
}

fn tls_config() -> Result<rustls::ClientConfig, String> {
    // SSL_CERT_FILE 给了就只信它（和 Python 的 ssl.create_default_context 一样）；没给用系统的
    if let Some(f) = std::env::var_os("SSL_CERT_FILE") {
        let pem = std::fs::read(&f).map_err(|e| e.to_string())?;
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls::pki_types::pem::PemObject::pem_slice_iter(&pem).flatten() {
            let _ = roots.add(c);
        }
        return Ok(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    }
    use rustls_platform_verifier::ConfigVerifierExt;
    rustls::ClientConfig::with_platform_verifier().map_err(|e| e.to_string())
}

async fn tls(host: &str, tcp: Box<dyn Stream>) -> Result<Box<dyn Stream>, String> {
    let cfg = tls_config()?;
    let conn = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?;
    let s = conn.connect(name, tcp).await.map_err(|e| e.to_string())?;
    Ok(Box::new(s))
}

async fn connect(host: &str, port: u16) -> Result<Imap, String> {
    let tcp = tokio::net::TcpStream::connect((host, port)).await.map_err(|e| e.to_string())?;
    let mut m = if port == 143 {
        let mut m = Imap { io: BufReader::new(Box::new(tcp) as Box<dyn Stream>), tag: 0, caps: vec![] };
        m.line().await.map_err(|e| format!("{e:?}"))?;
        let (st, why, _) = m.cmd("STARTTLS").await.map_err(|e| format!("{e:?}"))?;
        if st != "OK" {
            return Err(why);
        }
        let inner = m.io.into_inner();
        let t = tls(host, inner).await?;
        Imap { io: BufReader::new(t), tag: m.tag, caps: vec![] }
    } else {
        let t = tls(host, Box::new(tcp)).await?;
        let mut m = Imap { io: BufReader::new(t), tag: 0, caps: vec![] };
        let g = m.line().await.map_err(|e| format!("{e:?}"))?;
        let g = String::from_utf8_lossy(&g.text).into_owned();
        if !g.starts_with("* OK") && !g.starts_with("* PREAUTH") {
            return Err(g);
        }
        m
    };
    // imaplib 连上后先要 CAPABILITY
    let (_, _, lines) = m.cmd("CAPABILITY").await.map_err(|e| format!("{e:?}"))?;
    for l in lines {
        let s = String::from_utf8_lossy(&l.text).to_uppercase();
        if let Some(rest) = s.strip_prefix("* CAPABILITY ") {
            m.caps = rest.split_whitespace().map(String::from).collect();
        }
    }
    Ok(m)
}

fn untagged_num(lines: &[Line], code: &str) -> Option<i64> {
    let re = Regex::new(&format!(r"(?i)\[{code} (\d+)\]")).ok()?;
    lines.iter().rev().find_map(|l| re.captures(&String::from_utf8_lossy(&l.text)).and_then(|c| c[1].parse().ok()))
}

fn exists_of(lines: &[Line]) -> Option<i64> {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^\* (\d+) EXISTS").unwrap());
    lines.iter().rev().find_map(|l| R.captures(&String::from_utf8_lossy(&l.text)).and_then(|c| c[1].parse().ok()))
}

async fn fetch(m: &mut Imap, uid: i64, v: &Value) -> Result<Option<Value>, ImapErr> {
    let (_, _, lines) = m.cmd(&format!("UID FETCH {uid} (RFC822.SIZE BODY.PEEK[]<0.{MAX_FETCH}>)")).await?;
    let (mut raw, mut meta) = (vec![], vec![]);
    for l in lines {
        meta.extend_from_slice(&l.text);
        meta.push(b' ');
        if raw.is_empty() {
            if let Some(b) = l.literals.into_iter().next() {
                raw = b;
            }
        }
    }
    if raw.is_empty() {
        return Ok(None);
    }
    static SIZE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| regex::bytes::Regex::new(r"RFC822\.SIZE (\d+)").unwrap());
    let size = SIZE.captures(&meta).and_then(|c| std::str::from_utf8(&c[1]).ok()?.parse::<usize>().ok()).unwrap_or(raw.len());
    let mut names = vec![];
    if size > raw.len() {
        let (_, _, bs) = m.cmd(&format!("UID FETCH {uid} (BODYSTRUCTURE)")).await?;
        let mut all = vec![];
        for l in bs {
            all.extend_from_slice(&l.text);
            for x in l.literals {
                all.push(b' ');
                all.extend_from_slice(&x);
            }
            all.push(b' ');
        }
        names = attachments_in(&all);
    }
    let key = format!("{}-{uid}", py::str_of(v));
    Ok(Some(match message(&raw, size, &names, &key) {
        Ok(e) => e,
        Err(e) => json!({"id": key, "title": format!("收到一封新邮件（UID {uid}）"), "text": format!("这封信解析出错：{e}")}),
    }))
}

async fn run(params: &Value, host: &str, port: u16, state: &Value) -> Result<(Vec<Value>, Value), String> {
    let folder = if py::truthy(&params["folder"]) { py::str_of(&params["folder"]) } else { "INBOX".into() };
    let mut m = connect(host, port).await.map_err(|e| format!("连不上 {host}:{port}：{e}"))?;
    let r = session(&mut m, params, host, &folder, state).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), m.cmd("LOGOUT")).await;
    match r {
        Ok(x) => Ok(x),
        Err(Inner::Msg(s)) => Err(s),
        Err(Inner::Imap(ImapErr::Abort(s))) => Err(format!("和 {host} 的连接断了：{s}")),
        Err(Inner::Imap(ImapErr::Io(s))) => Err(format!("和 {host} 的连接出错：{s}")),
    }
}

enum Inner {
    Msg(String),
    Imap(ImapErr),
}

impl From<ImapErr> for Inner {
    fn from(e: ImapErr) -> Self {
        Inner::Imap(e)
    }
}

async fn session(m: &mut Imap, params: &Value, host: &str, folder: &str, state: &Value) -> Result<(Vec<Value>, Value), Inner> {
    let user = params["user"].as_str().unwrap_or("");
    let pass = params["password"].as_str().unwrap_or("");
    if !user.is_ascii() || !pass.is_ascii() {
        return Err(Inner::Msg("邮箱地址和授权码里不能有中文或全角字符".into()));
    }
    let (st, why, _) = m.cmd(&format!("LOGIN {} {}", quote(user), quote(pass))).await?;
    if st != "OK" {
        if why.to_lowercase().contains("unsafe login") {
            return Err(Inner::Msg(UNSAFE.into()));
        }
        let prov = if py::truthy(&params["provider"]) { py::str_of(&params["provider"]) } else { guess(user) };
        let h = hint(&prov).map(|h| format!("。{h}")).unwrap_or_default();
        let srv = if why.is_empty() { String::new() } else { format!("（服务器说：{}）", short(&why, 150)) };
        return Err(Inner::Msg(format!("{LOGIN_FAILED}{h}{srv}")));
    }
    if NEED_ID.iter().any(|d| host.ends_with(d)) || m.caps.iter().any(|c| c == "ID") {
        let _ = m.cmd("ID (\"name\" \"open-super-agent\" \"version\" \"1.0\" \"vendor\" \"open-super-agent\")").await;
    }
    let (st, why, lines) = m.cmd(&format!("EXAMINE {}", quote(&utf7(folder)))).await?;
    if st != "OK" {
        return Err(Inner::Msg(if why.to_lowercase().contains("unsafe login") { UNSAFE.into() } else { format!("打不开文件夹「{folder}」：{}", short(&why, 150)) }));
    }
    let v = untagged_num(&lines, "UIDVALIDITY").map(|x| json!(x)).unwrap_or(Value::Null);
    let nxt = untagged_num(&lines, "UIDNEXT");
    let exists = exists_of(&lines).unwrap_or(0);
    if !state.is_object() || state["v"] != v {
        let last = match nxt {
            Some(n) if n != 0 => n - 1,
            _ if exists == 0 => 0,
            _ => {
                let (_, _, ls) = m.cmd(&format!("FETCH {exists} (UID)")).await?;
                static U: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"UID (\d+)").unwrap());
                ls.iter().find_map(|l| U.captures(&String::from_utf8_lossy(&l.text)).and_then(|c| c[1].parse().ok())).unwrap_or(0)
            }
        };
        let mut sample = vec![];
        if !state.is_object() && last > 0 {
            if let Ok(Some(e)) = fetch(m, last, &v).await {
                sample.push(e);
            }
        }
        return Ok((sample, json!({"v": v, "last": last})));
    }
    let last = py::to_int(&state["last"]).unwrap_or(0);
    if let Some(n) = nxt.filter(|n| *n != 0) {
        if n - 1 <= last {
            return Ok((vec![], state.clone()));
        }
    }
    let (_, _, ls) = m.cmd(&format!("UID SEARCH UID {}:*", last + 1)).await?;
    let mut uids: Vec<i64> = vec![];
    for l in ls {
        let s = String::from_utf8_lossy(&l.text).into_owned();
        if let Some(rest) = s.strip_prefix("* SEARCH") {
            for x in rest.split_whitespace() {
                if let Ok(u) = x.parse::<i64>() {
                    if u > last && !uids.contains(&u) {
                        uids.push(u);
                    }
                }
            }
        }
    }
    uids.sort();
    if uids.is_empty() {
        return Ok((vec![], json!({"v": v, "last": last})));
    }
    let mut events = vec![];
    for u in &uids[uids.len().saturating_sub(MAX_NEW)..] {
        if let Some(e) = fetch(m, *u, &v).await? {
            events.push(e);
        }
    }
    if uids.len() > MAX_NEW {
        if let Some(l) = events.last_mut() {
            l["text"] = json!(format!("{}\n\n（这次一共来了 {} 封，只看了最新的 {MAX_NEW} 封）", py::str_of(&l["text"]), uids.len()));
        }
    }
    Ok((events, json!({"v": v, "last": uids[uids.len() - 1]})))
}

pub async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let (host, port) = server(&params)?;
    if !config::get_bool("tools.fetch_private", false) && tools::private_host(&format!("imap://{host}:{port}")).await {
        return Err(format!("没连：{host} 是本机或内网地址。要连内网的邮箱服务器，在 config.yaml 里设 tools.fetch_private: true"));
    }
    match tokio::time::timeout(Duration::from_secs(TIMEOUT * 6), run(&params, &host, port, &state)).await {
        Ok(r) => r,
        Err(_) => Err(format!("和 {host} 的连接出错：timed out")),
    }
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("mail");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.validate = Some(|mut p| {
        static HOST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]([a-z0-9\-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9\-]*[a-z0-9])?)+$").unwrap());
        let mut prov = if py::truthy(&p["provider"]) { py::str_of(&p["provider"]) } else { "auto".into() };
        if prov == "auto" {
            prov = guess(p["user"].as_str().unwrap_or(""));
            if prov.is_empty() {
                return Err("认不出是哪家的邮箱：在“邮箱服务商”里选一个，或者选“其他”自己填 IMAP 服务器".into());
            }
            p["provider"] = json!(prov);
        }
        if prov == "custom" {
            let (host, port) = host_port(p["host"].as_str().unwrap_or(""), &p["port"]);
            if !HOST.is_match(&host) {
                return Err("选了“其他”要填 IMAP 服务器的域名，比如 imap.example.com".into());
            }
            p["host"] = json!(host);
            p["port"] = json!(port);
        }
        if !p["user"].as_str().unwrap_or("").contains('@') {
            return Err("邮箱地址要写完整，比如 you@qq.com".into());
        }
        let f = if py::truthy(&p["folder"]) { py::str_of(&p["folder"]) } else { "INBOX".into() };
        p["folder"] = json!(f.trim());
        Ok(p)
    });
    s.summary = Some(|p| {
        let folder = if py::truthy(&p["folder"]) { py::str_of(&p["folder"]) } else { "INBOX".into() };
        let user = if py::truthy(&p["user"]) { py::str_of(&p["user"]) } else { String::new() };
        format!("{user}{}", if folder.to_uppercase() != "INBOX" { format!(" · {folder}") } else { String::new() })
    });
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message() {
        let raw = "From: =?UTF-8?B?5byg5LiJ?= <zhang@example.com>\r\nTo: a@b.com, \"Bob\" <bob@b.com>\r\nSubject: =?gb2312?B?suLK1A==?=\r\nDate: Thu, 19 Nov 2026 10:00:00 +0800\r\nMessage-ID: <x@y>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n5q2j5paHCgoKCuesrOS6jOautQ==\r\n--b\r\nContent-Type: application/pdf; name=\"=?UTF-8?B?5Y+R56WoLnBkZg==?=\"\r\nContent-Disposition: attachment; filename=\"=?UTF-8?B?5Y+R56WoLnBkZg==?=\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n--b--\r\n";
        let e = message(raw.as_bytes(), 0, &[], "7-3").unwrap();
        assert_eq!(e["id"], "7-3");
        assert_eq!(e["title"], "张三 来信：测试");
        let t = e["text"].as_str().unwrap();
        assert!(t.starts_with("发件人：张三 <zhang@example.com>\n收件人：a@b.com、Bob <bob@b.com>\n时间："), "{t}");
        assert!(t.contains("附件：发票.pdf\n\n正文：\n正文\n\n第二段"), "{t}");
        assert_eq!(utf7("已发送"), "&XfJT0ZAB-");
        assert_eq!(utf7("A&B"), "A&-B");
        assert_eq!(guess("x@QQ.com"), "qq");
        assert_eq!(attachments_in(br#"("NAME*" "utf-8''%E5%8F%91%E7%A5%A8.pdf")"#), vec!["发票.pdf".to_string()]);
    }
}
