//! 公网 IP 变了。对应 osa/sources/ip_change.py（说明见那里）。

use super::util;
use crate::py;
use crate::registry::Source;
use serde_json::{Value, json};
use std::net::IpAddr;

fn services(v: u8) -> &'static [&'static str] {
    if v == 4 {
        &["https://api.ipify.org", "https://ipv4.icanhazip.com", "https://ifconfig.me/ip"]
    } else {
        &["https://api6.ipify.org", "https://ipv6.icanhazip.com"]
    }
}

pub fn parse(text: &str, version: u8) -> Option<String> {
    let ip: IpAddr = text.trim().parse().ok()?;
    let ok = match ip {
        IpAddr::V4(_) => version == 4,
        IpAddr::V6(_) => version == 6,
    };
    if ok && crate::tools::is_global(ip) { Some(ip.to_string()) } else { None }
}

async fn lookup(version: u8, skip: &str) -> (Option<String>, String, Vec<String>) {
    let mut errors = vec![];
    for url in services(version) {
        if *url == skip {
            continue;
        }
        match util::get(url, 10.0, 10_000, &[], &[]).await {
            Err(e) => errors.push(format!("{url}：{}", e.text()).trim().to_string()),
            Ok(r) => match parse(&r.text(), version) {
                Some(ip) => return (Some(ip), url.to_string(), errors),
                None => errors.push(format!("{url}：返回的不是 IPv{version} 地址")),
            },
        }
    }
    (None, String::new(), errors)
}

fn prefix64(ip: &str) -> Option<u64> {
    match ip.parse::<IpAddr>().ok()? {
        IpAddr::V6(a) => Some((u128::from(a) >> 64) as u64),
        IpAddr::V4(_) => None,
    }
}

pub fn same(old: &str, new: &str, version: u8) -> bool {
    if version == 6 { prefix64(old) == prefix64(new) } else { old == new }
}

fn event(version: u8, old: &str, new: &str, by: &str, confirm: Option<&str>) -> Value {
    let what = if version == 6 { "IPv6 前缀" } else { "IPv4" };
    let when = py::strftime("%Y-%m-%d %H:%M");
    let tail = match confirm {
        None => "别的几家没连上、没能确认".to_string(),
        Some(c) if same(c, new, version) => "换了一家确认过".into(),
        Some(c) => format!("另一家查到的是 {c}"),
    };
    let how = format!("{} 查到，{tail}", by.strip_prefix("https://").unwrap_or(by));
    json!({"id": format!("ipv{version}:{old}->{new}"), "title": format!("公网 {what} 变了：{old} → {new}"),
           "text": format!("原来：{old}\n现在：{new}\n{when} 发现的（{how}）。\n有域名解析到这里（DDNS）、防火墙或别的服务按 IP 放行、端口转发写死了地址的，记得改。")})
}

async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let versions: Vec<u8> = if py::truthy(&params["ipv6"]) { vec![4, 6] } else { vec![4] };
    let old = if state.is_object() { state.clone() } else { json!({}) };
    let mut new = old.clone();
    let mut events = vec![];
    for v in versions {
        let (ip, by, errors) = lookup(v, "").await;
        let Some(ip) = ip else {
            if v == 4 {
                return Err(format!("几个查 IP 的服务都没答上：{}", errors.iter().take(3).cloned().collect::<Vec<_>>().join("；")));
            }
            continue;
        };
        let key = format!("v{v}");
        if let Some(o) = old[&key].as_str()
            && !same(o, &ip, v)
        {
            let (confirm, _, _) = lookup(v, &by).await;
            if confirm.as_deref().is_some_and(|c| same(c, o, v)) {
                continue;
            }
            events.push(event(v, o, &ip, &by, confirm.as_deref()));
        }
        new[key] = json!(ip);
    }
    Ok((events, new))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("ip_change");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.summary = Some(|p| if py::truthy(&p["ipv6"]) { "IPv4 和 IPv6".into() } else { String::new() });
    vec![s]
}
