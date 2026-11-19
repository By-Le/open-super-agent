//! 内置的事件来源（registry::Source）。对应 osa/sources/（写一个新来源的说明见那里的 __init__.py）。
//!
//! 写死的信息（标题、说明、参数、场景、示例）从 Python 版导出在 assets/sources.json（server/scripts/dump_routines.py），
//! 这里只写逻辑：check / parse / match / summary / validate。还没迁过来的来源照样登记（网页上看得到），
//! 但 available 说明 Rust 版还没有，不能新建。

pub mod feeds;
pub mod github_repo;
pub mod ip_change;
pub mod epic_free;
pub mod system;
pub mod util;
pub mod web;
pub mod webhook;

use crate::registry::{Context, Source};
use serde_json::Value;
use std::sync::LazyLock;

static META: LazyLock<Vec<Value>> = LazyLock::new(|| serde_json::from_str(include_str!("../../assets/sources.json")).expect("sources.json"));

/// 一个来源的骨架：写死的信息填好，逻辑留空
pub fn base(id: &str) -> Source {
    let meta = META.iter().find(|m| m["id"] == id).cloned().unwrap_or_else(|| panic!("assets/sources.json 里没有 {id}"));
    let mode = match meta["mode"].as_str() {
        Some("push") => crate::registry::PUSH,
        Some("poll") => crate::registry::POLL,
        _ => crate::registry::INTERNAL,
    };
    Source { id: id.into(), mode, meta, check: None, parse: None, matcher: None, validate: None, summary: None, available: None, owner: String::new() }
}

fn not_ported() -> String {
    "Rust 版还没有这种事件来源".into()
}

/// 还没迁过来的来源：登记上（列表、已有的事件照常显示），不能新建，检查时报错
fn stub(id: &str) -> Source {
    let mut s = base(id);
    s.available = Some(not_ported);
    if s.mode == crate::registry::POLL {
        s.check = Some(|_, _| Box::pin(async { Err(not_ported()) }));
    } else if s.mode == crate::registry::PUSH {
        s.parse = Some(|_, _| Err(crate::registry::ParseErr::Other(not_ported())));
    }
    s
}

pub fn register(ctx: &Context) {
    // 顺序和 Python 版一样（sources/__init__.py 的 ORDER）
    let mut all: Vec<Source> = vec![];
    all.extend(webhook::sources());
    all.extend(feeds::sources());
    all.extend(web::sources());
    all.extend(github_repo::sources());
    for id in ["mail", "calendar", "weather", "earthquake"] {
        all.push(stub(id));
    }
    all.extend(epic_free::sources());
    for id in ["price", "folder"] {
        all.push(stub(id));
    }
    all.extend(ip_change::sources());
    all.extend(system::sources());
    for s in all {
        if let Err(e) = ctx.source(s) {
            tracing::error!("{e}");
        }
    }
}
