//! 内置的事件来源（registry::Source）。对应 osa/sources/（写一个新来源的说明见那里的 __init__.py）。
//!
//! 写死的信息（标题、说明、参数、场景、示例）从 Python 版导出在 assets/sources.json（server/scripts/dump_routines.py），
//! 这里只写逻辑：check / parse / match / summary / validate。

pub mod calendar;
pub mod earthquake;
pub mod epic_free;
pub mod files;
pub mod mail;
pub mod market;
pub mod feeds;
pub mod github_repo;
pub mod ip_change;
pub mod system;
pub mod util;
pub mod web;
pub mod weather;
pub mod webhook;

use crate::registry::{Context, Source};
use serde_json::Value;
use std::sync::LazyLock;

static META: LazyLock<Vec<Value>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../../assets/sources.json")).expect("sources.json"));

/// 一个来源的骨架：写死的信息填好，逻辑留空
pub fn base(id: &str) -> Source {
    let meta = META.iter().find(|m| m["id"] == id).cloned().unwrap_or_else(|| panic!("assets/sources.json 里没有 {id}"));
    let mode = match meta["mode"].as_str() {
        Some("push") => crate::registry::PUSH,
        Some("poll") => crate::registry::POLL,
        _ => crate::registry::INTERNAL,
    };
    Source {
        id: id.into(),
        mode,
        meta,
        check: None,
        parse: None,
        matcher: None,
        validate: None,
        summary: None,
        available: None,
        owner: String::new(),
    }
}

pub fn register(ctx: &Context) {
    // 顺序和 Python 版一样（sources/__init__.py 的 ORDER）
    let mut all: Vec<Source> = vec![];
    all.extend(webhook::sources());
    all.extend(feeds::sources());
    all.extend(web::sources());
    all.extend(github_repo::sources());
    all.extend(mail::sources());
    all.extend(calendar::sources());
    all.extend(weather::sources());
    all.extend(earthquake::sources());
    all.extend(epic_free::sources());
    all.extend(market::sources());
    all.extend(files::sources());
    all.extend(ip_change::sources());
    all.extend(system::sources());
    for s in all {
        if let Err(e) = ctx.source(s) {
            tracing::error!("{e}");
        }
    }
}
