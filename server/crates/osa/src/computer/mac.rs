//! 派到你的 Mac 上做的任务用的电脑（osa/computer/mac.py）：每台登记过的 Mac 一个 Computer，后端是 Host 的 Mac 那一种
//! （host.rs 的 Place::Mac），动手的那一步经 link.rs 的连接交给那台 Mac 上的小程序。

use super::base::Computer;
use super::host::Host;
use crate::{config, i18n, security, server};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

static MACS: LazyLock<Mutex<BTreeMap<i64, Arc<Computer>>>> = LazyLock::new(Default::default);
static QUEUED: AtomicBool = AtomicBool::new(false);

/// 某台 Mac 的状态变了（连上、断开、谁在用、接手）：告诉前端（几件事前后脚发生时只发一次）
pub fn changed() {
    if QUEUED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(h) = tokio::runtime::Handle::try_current() else {
        QUEUED.store(false, Ordering::SeqCst);
        return;
    };
    h.spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        QUEUED.store(false, Ordering::SeqCst);
        server::broadcast(json!({"type": "macs", "macs": macs_state().await}));
    });
}

/// 那台 Mac 的电脑（不管连没连着；用的时候没连上会报错）
pub fn get(lid: i64) -> Arc<Computer> {
    MACS.lock()
        .entry(lid)
        .or_insert_with(|| {
            let c = Computer::new(0, Box::new(Host::mac(lid)), Some(Arc::new(changed)));
            {
                let mut s = c.st.lock();
                s.width = config::get_i64("computer.width", 1280);
                s.height = config::get_i64("computer.height", 800);
            }
            c
        })
        .clone()
}

pub fn drop_mac(lid: i64) {
    MACS.lock().remove(&lid);
}

pub fn computers() -> Vec<Arc<Computer>> {
    MACS.lock().values().cloned().collect()
}

/// 一台 Mac 给前端的样子
pub async fn mac_state(r: &Value) -> Value {
    let lid = r["id"].as_i64().unwrap_or(0);
    let lk = super::link::connected(lid);
    let mut out = r.clone();
    out["can_shell"] = json!(lk.as_ref().is_some_and(|l| l.can_shell));
    out["users"] = json!([]);
    out["controller"] = json!("bot");
    if lk.is_some() {
        let c = get(lid);
        let desk = match tokio::time::timeout(std::time::Duration::from_secs(10), c.backend.desk_state(&c)).await {
            Ok(d) => d,
            Err(_) => json!({"supported": true, "problem": i18n::t("「{name}」没有回应（超时）", &[("name", r["name"].clone())])}),
        };
        out["users"] = json!(super::users_of(&c));
        let s = c.st.lock();
        out["waiting"] = json!(s.waiting.values().cloned().collect::<Vec<_>>());
        out["controller"] = json!(s.controller);
        drop(s);
        out["desktop"] = desk;
    }
    out
}

/// 登记过的 Mac（派任务的去处）。演示服务器上没有
pub async fn macs_state() -> Vec<Value> {
    if security::demo() {
        return vec![];
    }
    let rows = super::link::rows();
    futures::future::join_all(rows.iter().map(mac_state)).await
}
