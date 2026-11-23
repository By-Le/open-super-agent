//! 数据备份与迁移：把全部数据打成一个加密文件（.osabak），换机器、从本机搬到服务器时恢复。对应 osa/backup.py（格式、步骤见那里）。
//!
//! 文件格式和 Python 版完全一样：第一行 "OSABAK1"，第二行 JSON 头（scrypt 参数、salt、nonce 前缀、块大小），后面是加密的 zip；
//! zip 按 1 MiB 分块，AES-256-GCM，nonce = 前缀 7 字节 + 块号 4 字节 + 是否最后一块 1 字节，头的两行作附加数据。两个版本的备份互相能恢复。

use crate::web::{ApiError, Res, ok};
use crate::{config, py, security, server, vault};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::body::Body as RawBody;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::Response;
use axum::routing::{get, post, put};
use parking_lot::Mutex;
use regex::Regex;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub const MAGIC: &[u8] = b"OSABAK1\n";
pub const CHUNK: usize = 1 << 20;
pub const FORMAT: i64 = 1;
pub const MIN_PASS: usize = 8;
pub const MAX_UNZIPPED: u64 = 8 << 30;
pub const MAX_BACKUP: u64 = 4 << 30;
const NEED_TABLES: [&str; 3] = ["bots", "chats", "messages"];
const COUNTS: [(&str, &str); 9] = [
    ("bots", "个 Bot"),
    ("chats", "个对话"),
    ("messages", "条消息"),
    ("memories", "条记忆"),
    ("skills", "个技能"),
    ("routines", "个定时任务"),
    ("pages", "个页面"),
    ("apps", "个应用"),
    ("channels", "个聊天软件连接"),
];

/// 网页上确认恢复后置位：服务关闭时重新启动自己
pub static RESTART: AtomicBool = AtomicBool::new(false);
/// 服务启动的时间（/api/backup 的 started）
pub static STARTED: Mutex<f64> = Mutex::new(0.0);

fn db_path() -> PathBuf {
    config::DB_PATH.clone()
}

fn stage_dir() -> PathBuf {
    db_path().parent().map(|d| d.join("restore")).unwrap_or_else(|| "restore".into())
}

fn apps_dir() -> PathBuf {
    config::DATA.join("apps")
}

fn docs_dir() -> PathBuf {
    config::DATA.join("docs")
}

/// 文档服务的数据：数据库 oad.db 和上传的文件 blobs/。搜索索引 search/ 不带，文档服务启动时自己补
fn docs_files(dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut es: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        es.sort();
        for p in es {
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.is_file()
                && let Ok(rel) = p.strip_prefix(base)
            {
                out.push((format!("docs/{}", rel.to_string_lossy().replace('\\', "/")), p.clone()));
            }
        }
    }
    let mut out = vec![];
    walk(dir, &dir.join("blobs"), &mut out);
    out
}

/// 备份里文档的文件名：docs/oad.db 或 docs/blobs/…（每段只有字母数字和 . _ -，不能是 . 或 ..）
fn docs_entry_ok(n: &str) -> bool {
    static SEG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]{1,128}$").unwrap());
    if n == "docs/oad.db" {
        return true;
    }
    let Some(rest) = n.strip_prefix("docs/blobs/") else { return false };
    let segs: Vec<&str> = rest.split('/').collect();
    segs.len() <= 8 && segs.iter().all(|x| SEG.is_match(x) && *x != "." && *x != "..")
}

type R<T> = Result<T, String>;

fn io<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn key(passphrase: &str, head: &Value) -> R<Aes256Gcm> {
    let salt = crate::util::b64_decode(head["salt"].as_str().unwrap_or("")).ok_or_else(|| t!("备份文件的开头坏了"))?;
    let n = head["n"].as_u64().unwrap_or(0);
    let log_n = 63 - n.leading_zeros() as u8;
    if n == 0 || 1u64 << log_n != n {
        return Err(t!("备份文件的开头坏了"));
    }
    let p = scrypt::Params::new(log_n, head["r"].as_u64().unwrap_or(8) as u32, head["p"].as_u64().unwrap_or(1) as u32, 32)
        .map_err(|_| t!("备份文件的开头坏了"))?;
    let mut out = [0u8; 32];
    scrypt::scrypt(passphrase.as_bytes(), &salt, &p, &mut out).map_err(io)?;
    Aes256Gcm::new_from_slice(&out).map_err(io)
}

fn nonce(prefix: &[u8], i: u32, last: bool) -> Vec<u8> {
    let mut n = prefix.to_vec();
    n.extend_from_slice(&i.to_be_bytes());
    n.push(last as u8);
    n
}

pub fn check_passphrase(p: &str) -> R<()> {
    if p.chars().count() < MIN_PASS {
        return Err(t!("口令至少 {n} 个字符：备份里有你所有的密钥，口令就是保护它们的锁", n = MIN_PASS));
    }
    Ok(())
}

fn read_full(f: &mut impl Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

// ---------- 加密、解密 ----------
pub fn encrypt(src: &Path, dst: &Path, passphrase: &str) -> R<()> {
    let salt: Vec<u8> = (0..16).map(|_| rand::random::<u8>()).collect();
    let prefix: Vec<u8> = (0..7).map(|_| rand::random::<u8>()).collect();
    let head = json!({"kdf": "scrypt", "n": 1 << 15, "r": 8, "p": 1, "salt": crate::util::b64(&salt), "nonce": crate::util::b64(&prefix), "chunk": CHUNK});
    let mut hline = py::dumps(&head, true).into_bytes();
    hline.push(b'\n');
    let aes = key(passphrase, &head)?;
    let mut ad = MAGIC.to_vec();
    ad.extend_from_slice(&hline);
    let mut f = std::fs::File::open(src).map_err(io)?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(dst).map_err(io)?);
    out.write_all(&ad).map_err(io)?;
    let mut i: u32 = 0;
    let mut buf = read_full(&mut f, CHUNK).map_err(io)?;
    loop {
        let nxt = read_full(&mut f, CHUNK).map_err(io)?;
        let n = nonce(&prefix, i, nxt.is_empty());
        let ct = aes.encrypt(Nonce::from_slice(&n), Payload { msg: &buf, aad: &ad }).map_err(|_| "encrypt".to_string())?;
        out.write_all(&ct).map_err(io)?;
        if nxt.is_empty() {
            return out.flush().map_err(io);
        }
        buf = nxt;
        i += 1;
    }
}

pub fn decrypt(src: &Path, dst: &Path, passphrase: &str) -> R<()> {
    let mut f = std::io::BufReader::new(std::fs::File::open(src).map_err(io)?);
    if read_full(&mut f, MAGIC.len()).map_err(io)? != MAGIC {
        return Err(t!("这不是 open-super-agent 的备份文件"));
    }
    // readline(2048)
    let mut hline = vec![];
    while hline.len() < 2048 {
        let mut b = [0u8; 1];
        if f.read(&mut b).map_err(io)? == 0 {
            break;
        }
        hline.push(b[0]);
        if b[0] == b'\n' {
            break;
        }
    }
    let head: Value = serde_json::from_slice(&hline).unwrap_or(Value::Null);
    let n = head["n"].as_u64().unwrap_or(0);
    let chunk = head["chunk"].as_u64().unwrap_or(0);
    let good = head["kdf"] == "scrypt"
        && (1 << 10..=1 << 20).contains(&n)
        && head["r"].as_u64().is_some_and(|r| r <= 16)
        && head["p"].as_u64().is_some_and(|p| p <= 4)
        && (1 << 12..=1 << 24).contains(&chunk);
    let prefix = head["nonce"].as_str().and_then(crate::util::b64_decode).unwrap_or_default();
    if !good || prefix.len() != 7 {
        return Err(t!("备份文件的开头坏了"));
    }
    let aes = key(passphrase, &head)?;
    let mut ad = MAGIC.to_vec();
    ad.extend_from_slice(&hline);
    let size = chunk as usize + 16;
    let mut out = std::io::BufWriter::new(std::fs::File::create(dst).map_err(io)?);
    let mut i: u32 = 0;
    let mut buf = read_full(&mut f, size).map_err(io)?;
    loop {
        let nxt = read_full(&mut f, size).map_err(io)?;
        let n = nonce(&prefix, i, nxt.is_empty());
        match aes.decrypt(Nonce::from_slice(&n), Payload { msg: &buf, aad: &ad }) {
            Ok(pt) => out.write_all(&pt).map_err(io)?,
            Err(_) => {
                return Err(if i == 0 { t!("口令不对（或者文件坏了）") } else { t!("备份文件不完整，或者被改过") });
            }
        }
        if nxt.is_empty() {
            return out.flush().map_err(io);
        }
        buf = nxt;
        i += 1;
    }
}

// ---------- 导出 ----------
pub fn version() -> String {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?m)^version\s*=\s*"([^"]+)""#).unwrap());
    match std::fs::read_to_string(config::ROOT.join("pyproject.toml")) {
        Ok(s) => RE.captures(&s).map(|c| c[1].to_string()).unwrap_or_default(),
        Err(_) => env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn tables(db: &Connection) -> Vec<String> {
    db.prepare("select name from sqlite_master where type='table'")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|m| m.filter_map(|x| x.ok()).collect()))
        .unwrap_or_default()
}

pub fn counts(db: &Connection) -> Value {
    let have = tables(db);
    let mut o = serde_json::Map::new();
    for (t, _) in COUNTS {
        if have.iter().any(|h| h == t) {
            let n: i64 = db.query_row(&format!("select count(*) from {t}"), [], |r| r.get(0)).unwrap_or(0);
            o.insert(t.into(), json!(n));
        }
    }
    Value::Object(o)
}

fn mkdtemp(prefix: &str) -> R<PathBuf> {
    for _ in 0..10 {
        let d = std::env::temp_dir().join(format!("{prefix}{}", crate::util::token_hex(4)));
        if std::fs::create_dir(&d).is_ok() {
            return Ok(d);
        }
    }
    Err("mkdtemp".into())
}

/// 数据库的一致快照（服务照常运行），放在临时目录里
pub fn snapshot() -> R<PathBuf> {
    let work = mkdtemp("osa-backup-")?;
    let p = work.join("osa.db");
    if let Err(e) = crate::db::conn().execute("vacuum into ?", params![p.to_string_lossy()]) {
        let _ = std::fs::remove_dir_all(&work);
        return Err(io(e));
    }
    Ok(p)
}

/// 打一个备份，返回 (文件路径, 摘要)。文件在临时目录里，用完由调用方删掉整个目录
pub fn export(passphrase: &str, snap: PathBuf) -> R<(PathBuf, Value)> {
    let work = snap.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let r = (|| -> R<(PathBuf, Value)> {
        check_passphrase(passphrase)?; // 出错时整个临时目录删掉，传进来的快照也算
        let db = Connection::open(&snap).map_err(io)?;
        let mut secrets = serde_json::Map::new();
        let mut unreadable = vec![];
        let rows: Vec<(String, Vec<u8>)> = db
            .prepare("select name, ciphertext from vault")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1).unwrap_or_default())))
                    .map(|m| m.filter_map(|x| x.ok()).collect())
            })
            .unwrap_or_default();
        for (name, ct) in rows {
            match vault::decrypt_raw(&ct) {
                Some(v) => {
                    secrets.insert(name, json!(v));
                }
                None => unreadable.push(name),
            }
        }
        let _ = db.execute("delete from vault", []);
        crate::docs::tokens_for_backup(&db);
        // 文档服务自己的数据库：另做一份一致的快照（文档服务照常运行）
        let docs = crate::docs::local_dir();
        let docs_snap = work.join("oad.db");
        if let Some(d) = &docs {
            // 文档服务把编辑攒 20 毫秒左右再存盘：稍等一下，刚改的也带上
            std::thread::sleep(std::time::Duration::from_millis(200));
            let src = Connection::open(d.join("oad.db")).map_err(io)?;
            let _ = src.busy_timeout(std::time::Duration::from_secs(10));
            src.execute("vacuum into ?", params![docs_snap.to_string_lossy()]).map_err(io)?;
        }
        let mut meta = json!({"format": FORMAT, "created": py::now(), "version": version(), "counts": counts(&db),
                              "secrets": secrets.len(), "unreadable": unreadable});
        if docs.is_some() {
            meta["counts"]["docs"] = json!(crate::docs::page_count(&docs_snap).unwrap_or(0));
        }
        let _ = db.execute_batch("vacuum");
        drop(db);
        let mut apps: Vec<PathBuf> = std::fs::read_dir(apps_dir())
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "yaml")).collect())
            .unwrap_or_default();
        apps.sort();
        meta["local_apps"] = json!(apps.len());
        let zpath = work.join("backup.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zpath).map_err(io)?);
            let o = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
            z.start_file("backup.json", o).map_err(io)?;
            z.write_all(py::dumps_indent(&meta, 1, false).as_bytes()).map_err(io)?;
            z.start_file("secrets.json", o).map_err(io)?;
            z.write_all(py::dumps_u(&Value::Object(secrets)).as_bytes()).map_err(io)?;
            z.start_file("osa.db", o).map_err(io)?;
            std::io::copy(&mut std::fs::File::open(&snap).map_err(io)?, &mut z).map_err(io)?;
            for p in &apps {
                z.start_file(format!("apps/{}", p.file_name().unwrap_or_default().to_string_lossy()), o).map_err(io)?;
                std::io::copy(&mut std::fs::File::open(p).map_err(io)?, &mut z).map_err(io)?;
            }
            if let Some(d) = &docs {
                z.start_file("docs/oad.db", o).map_err(io)?;
                std::io::copy(&mut std::fs::File::open(&docs_snap).map_err(io)?, &mut z).map_err(io)?;
                for (name, p) in docs_files(d) {
                    if !docs_entry_ok(&name) {
                        continue;
                    }
                    let Ok(mut f) = std::fs::File::open(&p) else { continue }; // 刚被删掉的文件
                    z.start_file(name, o).map_err(io)?;
                    std::io::copy(&mut f, &mut z).map_err(io)?;
                }
            }
            z.finish().map_err(io)?;
        }
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&docs_snap);
        let out = work.join(format!("osa-backup-{}.osabak", py::strftime("%Y%m%d-%H%M")));
        encrypt(&zpath, &out, passphrase)?;
        let _ = std::fs::remove_file(&zpath);
        Ok((out, meta))
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&work);
    }
    r
}

/// 命令行导出：`osa-server backup export [-o 目录或文件]`，口令放在 OSA_BACKUP_PASSPHRASE 里（方便定时备份、Docker 里用）。
/// 服务开着也能导出（数据库做的是一致快照）。恢复在网页上做：设置 → 备份与迁移
pub fn cli() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(2).collect();
    if a.first().map(String::as_str) != Some("export") {
        anyhow::bail!("用法：osa-server backup export [-o 目录或文件]（口令放在环境变量 OSA_BACKUP_PASSPHRASE 里；恢复在网页的 设置 → 备份与迁移）");
    }
    let out = a.iter().position(|x| x == "-o" || x == "--output").and_then(|i| a.get(i + 1)).map(PathBuf::from).unwrap_or_else(|| ".".into());
    let pass = std::env::var("OSA_BACKUP_PASSPHRASE").unwrap_or_default();
    if pass.is_empty() {
        anyhow::bail!("请把备份口令放在环境变量 OSA_BACKUP_PASSPHRASE 里");
    }
    check_passphrase(&pass).map_err(anyhow::Error::msg)?;
    if !db_path().exists() {
        anyhow::bail!("没有数据：{} 不存在（OSA_DATA_DIR 对吗？）", db_path().display());
    }
    let (path, meta) = export(&pass, snapshot().map_err(anyhow::Error::msg)?).map_err(anyhow::Error::msg)?;
    let dst = if out.is_dir() { out.join(path.file_name().unwrap_or_default()) } else { out };
    let r = std::fs::copy(&path, &dst).or_else(|_| std::fs::rename(&path, &dst).map(|_| 0));
    if let Some(d) = path.parent() {
        let _ = std::fs::remove_dir_all(d);
    }
    r?;
    println!("已导出到 {}：{}", dst.display(), meta["counts"]);
    Ok(())
}

// ---------- 恢复 ----------
/// 已经解密检查好、等确认的备份的摘要
pub fn staged() -> Option<Value> {
    let s = stage_dir();
    if !s.join("meta.json").exists() || s.join("ready").exists() {
        return None;
    }
    serde_json::from_str(&std::fs::read_to_string(s.join("meta.json")).ok()?).ok()
}

/// 解密、检查备份，放到 data/restore/ 等确认。返回摘要
pub fn stage(upload: &Path, passphrase: &str) -> R<Value> {
    let work = mkdtemp("osa-restore-")?;
    let stage = stage_dir();
    let r = (|| -> R<Value> {
        let zpath = work.join("backup.zip");
        decrypt(upload, &zpath, passphrase)?;
        let bad = || t!("备份文件里的内容坏了");
        let mut z = zip::ZipArchive::new(std::fs::File::open(&zpath).map_err(io)?).map_err(|_| bad())?;
        let names: Vec<String> = z.file_names().map(String::from).collect();
        let meta: Value = {
            let mut s = String::new();
            z.by_name("backup.json").map_err(|_| bad())?.read_to_string(&mut s).map_err(|_| bad())?;
            serde_json::from_str(&s).map_err(|_| bad())?
        };
        if meta["format"].as_i64() != Some(FORMAT) {
            return Err(t!("这个备份是更新版本的 open-super-agent 做的，先升级再恢复"));
        }
        let total: u64 = (0..z.len()).filter_map(|i| z.by_index_raw(i).ok().map(|f| f.size())).sum();
        if !names.iter().any(|n| n == "osa.db") || total > MAX_UNZIPPED {
            return Err(t!("备份文件里的内容不对"));
        }
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage).map_err(io)?;
        {
            let mut src = z.by_name("osa.db").map_err(|_| bad())?;
            let mut dst = std::fs::File::create(stage.join("osa.db")).map_err(io)?;
            std::io::copy(&mut src, &mut dst).map_err(io)?;
        }
        let db = Connection::open(stage.join("osa.db")).map_err(io)?;
        let ok: String = db.query_row("pragma integrity_check", [], |r| r.get(0)).map_err(|_| t!("备份里的数据库坏了"))?;
        if ok != "ok" {
            return Err(t!("备份里的数据库坏了"));
        }
        let have = tables(&db);
        if NEED_TABLES.iter().any(|t| !have.iter().any(|h| h == t)) {
            return Err(t!("备份里不是 open-super-agent 的数据"));
        }
        let secrets: Value = if names.iter().any(|n| n == "secrets.json") {
            let mut s = String::new();
            z.by_name("secrets.json").map_err(|_| bad())?.read_to_string(&mut s).map_err(|_| bad())?;
            serde_json::from_str(&s).map_err(|_| bad())?
        } else {
            json!({})
        };
        db.execute_batch("create table if not exists vault(name text primary key, ciphertext blob, created real); delete from vault;")
            .map_err(io)?;
        for (name, value) in secrets.as_object().cloned().unwrap_or_default() {
            db.execute("insert into vault values(?,?,?)", params![name, vault::encrypt_raw(&py::str_of(&value)), py::now()]).map_err(io)?;
        }
        if have.iter().any(|h| h == "computers") {
            let _ = db.execute("delete from computers", []);
        }
        let with_docs = names.iter().any(|n| n == "docs/oad.db");
        crate::docs::tokens_from_backup(&db, with_docs);
        let off = if have.iter().any(|h| h == "channels") {
            db.execute("update channels set enabled=0 where enabled=1", []).unwrap_or(0)
        } else {
            0
        };
        let mut info = meta.clone();
        info["counts"] = counts(&db);
        info["channels_off"] = json!(off);
        drop(db);
        static APP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^apps/[a-z][a-z0-9_-]{0,23}\.yaml$").unwrap());
        let apps: Vec<&String> = names.iter().filter(|n| APP.is_match(n)).collect();
        if !apps.is_empty() {
            std::fs::create_dir(stage.join("apps")).map_err(io)?;
            for n in apps {
                let mut b = vec![];
                z.by_name(n).map_err(|_| bad())?.read_to_end(&mut b).map_err(io)?;
                std::fs::write(stage.join("apps").join(&n[5..]), b).map_err(io)?;
            }
        }
        if with_docs {
            for n in names.iter().filter(|n| docs_entry_ok(n)) {
                let dst = stage.join(n);
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p).map_err(io)?;
                }
                let mut src = z.by_name(n).map_err(|_| bad())?;
                std::io::copy(&mut src, &mut std::fs::File::create(&dst).map_err(io)?).map_err(io)?;
            }
            let odb = stage.join("docs/oad.db");
            let ok: String = Connection::open(&odb)
                .and_then(|c| c.query_row("pragma integrity_check", [], |r| r.get(0)))
                .map_err(|_| t!("备份里的数据库坏了"))?;
            if ok != "ok" {
                return Err(t!("备份里的数据库坏了"));
            }
            info["counts"]["docs"] = json!(crate::docs::page_count(&odb).unwrap_or(0));
        }
        std::fs::write(stage.join("meta.json"), py::dumps_u(&info)).map_err(io)?;
        Ok(info)
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    let _ = std::fs::remove_dir_all(&work);
    r
}

pub fn cancel() {
    let _ = std::fs::remove_dir_all(stage_dir());
}

/// 标记好，下次启动时换上
pub fn confirm() -> R<()> {
    let s = stage_dir();
    if !s.join("meta.json").exists() {
        return Err(t!("没有等待恢复的备份，请重新上传"));
    }
    std::fs::write(s.join("ready"), py::now().to_string()).map_err(io)
}

/// 服务启动、打开数据库之前调用：有确认过的备份就换上
pub fn apply_pending() -> bool {
    {
        let mut st = STARTED.lock();
        if *st == 0.0 {
            *st = py::now();
        }
    }
    let s = stage_dir();
    if !s.join("ready").exists() || !s.join("osa.db").exists() {
        return false;
    }
    let db = db_path();
    let ts = py::strftime("%Y%m%d-%H%M%S");
    let name = db.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let old = db.with_file_name(format!("{name}.before-restore-{ts}"));
    if db.exists() {
        let _ = std::fs::rename(&db, &old);
    }
    for suf in ["-wal", "-shm", "-journal"] {
        let p = PathBuf::from(format!("{}{suf}", db.display()));
        if p.exists() {
            let _ = std::fs::rename(&p, format!("{}{suf}", old.display()));
        }
    }
    let _ = std::fs::rename(s.join("osa.db"), &db);
    let apps = apps_dir();
    if s.join("apps").is_dir() {
        if apps.exists() {
            let _ = std::fs::rename(&apps, apps.with_file_name(format!("apps.before-restore-{ts}")));
        }
        let _ = std::fs::rename(s.join("apps"), &apps);
    }
    // 文档整个换成备份里的；备份里没有文档就从空的开始（原来的都留着）
    let docs = docs_dir();
    if docs.exists() {
        let _ = std::fs::rename(&docs, docs.with_file_name(format!("docs.before-restore-{ts}")));
    }
    if s.join("docs").is_dir() {
        let _ = std::fs::rename(s.join("docs"), &docs);
    }
    let _ = std::fs::remove_dir_all(&s);
    println!("已从备份恢复数据，原来的数据库留在 {}", old.display());
    true
}

/// 让服务正常关闭（回完这个请求），关闭的最后一步重新启动自己（restart）
pub fn request_restart(delay: f64) {
    RESTART.store(true, Ordering::SeqCst);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
        let _ = server::SHUTDOWN.0.send(true);
    });
}

/// 在服务关闭的最后调用：要重启的话用同样的命令行重新执行自己（systemd、Docker、直接运行都适用）。不用重启就什么都不做
pub fn restart_if_requested() {
    if !RESTART.load(Ordering::SeqCst) {
        return;
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut args = std::env::args_os();
        let exe = std::env::current_exe().ok().map(|p| p.into_os_string()).or_else(|| args.next()).unwrap_or_default();
        let rest: Vec<_> = std::env::args_os().skip(1).collect();
        let err = std::process::Command::new(exe).args(rest).exec();
        eprintln!("重新启动失败：{err}");
        std::process::exit(3);
    }
    #[cfg(not(unix))]
    {
        let rest: Vec<_> = std::env::args_os().skip(1).collect();
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).args(rest).spawn();
        }
        std::process::exit(0);
    }
}

// ---------- 接口 ----------
fn no_backup_in_demo() -> Result<(), ApiError> {
    if security::demo() {
        return Err(ApiError::new(403, t!("演示服务器上不能导出或恢复备份")));
    }
    Ok(())
}

async fn backup_info() -> Res {
    let mut c = counts(&crate::db::conn());
    if let Some(n) = crate::docs::local_dir().and_then(|d| crate::docs::page_count(&d.join("oad.db"))) {
        c["docs"] = json!(n);
    }
    ok(json!({"counts": c, "secrets": vault::names().len(), "staged": staged(), "started": *STARTED.lock(), "version": version()}))
}

async fn backup_export(crate::web::Body(d): crate::web::Body) -> Result<Response, ApiError> {
    no_backup_in_demo()?;
    let pass = d["passphrase"].as_str().unwrap_or("").to_string();
    check_passphrase(&pass)?;
    let snap = snapshot()?;
    let (path, meta) = tokio::task::spawn_blocking(move || export(&pass, snap)).await.map_err(|e| ApiError::new(500, e.to_string()))??;
    let data = tokio::fs::read(&path).await.map_err(|e| ApiError::new(500, e.to_string()));
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
    let data = data?;
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut r = Response::new(axum::body::Body::from(data));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert("x-backup-secrets", HeaderValue::from(meta["secrets"].as_u64().unwrap_or(0)));
    h.insert("x-backup-unreadable", HeaderValue::from(meta["unreadable"].as_array().map(|a| a.len()).unwrap_or(0) as u64));
    Ok(r)
}

async fn backup_upload(h: HeaderMap, body: RawBody) -> Res {
    use futures::StreamExt;
    no_backup_in_demo()?;
    if RESTART.load(Ordering::SeqCst) {
        return Err(ApiError::new(409, t!("正在重启，稍后再试")));
    }
    let pass = h
        .get("x-passphrase")
        .and_then(|v| v.to_str().ok())
        .and_then(crate::util::b64_decode)
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    if pass.is_empty() {
        return Err(ApiError::bad(t!("请填备份时设的口令")));
    }
    let tmp = std::env::temp_dir().join(format!("{}.osabak", crate::util::token_hex(8)));
    let r = async {
        let mut f = tokio::fs::File::create(&tmp).await.map_err(|e| ApiError::new(500, e.to_string()))?;
        let mut size: u64 = 0;
        let mut s = body.into_data_stream();
        while let Some(chunk) = s.next().await {
            let chunk = chunk.map_err(|e| ApiError::bad(e.to_string()))?;
            size += chunk.len() as u64;
            if size > MAX_BACKUP {
                return Err(ApiError::new(413, t!("文件太大了")));
            }
            tokio::io::AsyncWriteExt::write_all(&mut f, &chunk).await.map_err(|e| ApiError::new(500, e.to_string()))?;
        }
        tokio::io::AsyncWriteExt::flush(&mut f).await.map_err(|e| ApiError::new(500, e.to_string()))?;
        if size == 0 {
            return Err(ApiError::bad(t!("没有收到文件")));
        }
        let p = tmp.clone();
        let info = tokio::task::spawn_blocking(move || stage(&p, &pass)).await.map_err(|e| ApiError::new(500, e.to_string()))??;
        Ok(axum::Json(info))
    }
    .await;
    let _ = std::fs::remove_file(&tmp);
    r
}

async fn backup_confirm() -> Res {
    no_backup_in_demo()?;
    confirm()?;
    server::broadcast(json!({"type": "restarting"}));
    request_restart(0.5);
    ok(json!({"restarting": true}))
}

async fn backup_cancel() -> Res {
    cancel();
    ok(json!({"ok": true}))
}

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/api/backup", get(backup_info))
        .route("/api/backup/export", post(backup_export))
        .route("/api/backup/restore", put(backup_upload).delete(backup_cancel).layer(DefaultBodyLimit::disable()))
        .route("/api/backup/restore/confirm", post(backup_confirm))
}
