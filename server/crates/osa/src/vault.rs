//! 保险库：加密保存密钥（Fernet，和 Python 版同一种格式、同一把主密钥）。对应 osa/vault.py。

use crate::db::{self, RowExt};
use crate::{config, py};
use regex::Regex;
use rusqlite::params;
use serde_json::{Value, json};
use std::sync::{LazyLock, OnceLock};

pub fn migrate() {
    db::batch("create table if not exists vault(name text primary key, ciphertext blob, created real)");
}

static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,63}$").unwrap());
static KEY: OnceLock<fernet::Fernet> = OnceLock::new();

fn key() -> &'static fernet::Fernet {
    KEY.get_or_init(|| {
        let raw = match std::env::var("OSA_VAULT_KEY").ok().filter(|v| !v.is_empty()) {
            Some(v) => v,
            None => {
                let f = config::DB_PATH.parent().map(|d| d.join("vault.key")).unwrap_or_else(|| "vault.key".into());
                if !f.exists() {
                    let _ = std::fs::write(&f, fernet::Fernet::generate_key());
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600));
                    }
                }
                std::fs::read_to_string(&f).unwrap_or_default().trim().to_string()
            }
        };
        fernet::Fernet::new(&raw).unwrap_or_else(|| {
            // 不是 Fernet 格式的口令：派生一个
            use sha2::Digest;
            let d = sha2::Sha256::digest(raw.as_bytes());
            let k = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE, d);
            fernet::Fernet::new(&k).expect("fernet key")
        })
    })
}

pub fn put(name: &str, value: &str) -> Result<(), String> {
    if !NAME.is_match(name) {
        return Err(t!("名字只能用字母、数字和下划线，且不能以数字开头"));
    }
    if value.is_empty() {
        return Err(t!("值不能为空"));
    }
    let token = key().encrypt(value.as_bytes());
    db::exec("insert or replace into vault values(?,?,?)", params![name, token.into_bytes(), py::now()]);
    Ok(())
}

pub fn get(name: &str) -> Option<String> {
    let r = db::row("select ciphertext from vault where name=?", params![name])?;
    let token = r.s(0);
    key().decrypt(&token).ok().and_then(|b| String::from_utf8(b).ok())
}

pub fn delete(name: &str) {
    db::exec("delete from vault where name=?", params![name]);
}

pub fn names() -> Vec<Value> {
    db::rows("select name, created from vault order by name", [])
        .into_iter()
        .map(|r| json!({"name": r[0], "created": r[1], "ok": get(&r.s(0)).is_some()}))
        .collect()
}

/// 用这台机器的主密钥加密（备份恢复时把明文写进待换上的数据库用）
pub fn encrypt_raw(value: &str) -> Vec<u8> {
    key().encrypt(value.as_bytes()).into_bytes()
}

/// 解开一条密文（备份导出时读快照里的保险库用）；主密钥换过、解不开返回 None
pub fn decrypt_raw(token: &[u8]) -> Option<String> {
    key().decrypt(std::str::from_utf8(token).ok()?).ok().and_then(|b| String::from_utf8(b).ok())
}
