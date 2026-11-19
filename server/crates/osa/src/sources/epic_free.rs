//! Epic 免费游戏。对应 osa/sources/epic_free.py（说明见那里）。

use super::util;
use crate::py;
use crate::registry::Source;
use chrono::{DateTime, Local, Utc};
use serde_json::{Value, json};

const API: &str = "https://store-site-backend-static.ak.epicgames.com/freeGamesPromotions";
const STORE: &str = "https://store.epicgames.com/zh-CN";
const MAX_SEEN: usize = 200;
const COUNTRIES: [(&str, &str); 5] = [("CN", "中国大陆"), ("HK", "香港"), ("TW", "台湾"), ("US", "美国"), ("JP", "日本")];

fn when(s: &Value) -> Option<DateTime<Utc>> {
    let s = s.as_str()?.replace('Z', "+00:00");
    DateTime::parse_from_rfc3339(&s).ok().map(|d| d.with_timezone(&Utc))
}

fn free_offers(groups: &Value) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = vec![];
    for g in groups.as_array().cloned().unwrap_or_default() {
        for o in g["promotionalOffers"].as_array().cloned().unwrap_or_default() {
            if o["discountSetting"]["discountPercentage"] != json!(0) {
                continue;
            }
            if let (Some(a), Some(b)) = (when(&o["startDate"]), when(&o["endDate"])) {
                out.push((a, b));
            }
        }
    }
    out
}

pub fn link(el: &Value) -> String {
    let mut slug = String::new();
    let maps: Vec<Value> = el["catalogNs"]["mappings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .chain(el["offerMappings"].as_array().cloned().unwrap_or_default())
        .collect();
    for m in maps {
        if py::truthy(&m["pageSlug"]) && m["pageType"] == "productHome" {
            slug = py::str_of(&m["pageSlug"]);
            break;
        }
    }
    if slug.is_empty() {
        let p = if py::truthy(&el["productSlug"]) { py::str_of(&el["productSlug"]) } else { String::new() };
        slug = p.strip_suffix("/home").unwrap_or(&p).to_string();
    }
    if slug.is_empty() {
        return format!("{STORE}/free-games");
    }
    format!("{STORE}/{}/{slug}", if el["offerType"] == "BUNDLE" { "bundles" } else { "p" })
}

fn day(d: &DateTime<Utc>) -> String {
    let d = d.with_timezone(&Local);
    format!("{}月{}日 {}", d.format("%-m"), d.format("%-d"), d.format("%H:%M"))
}

fn iso(d: &DateTime<Utc>) -> String {
    d.format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string().replace(".000+00:00", "+00:00")
}

pub fn games(data: &Value, now: DateTime<Utc>) -> (Vec<Value>, Vec<Value>) {
    let els = data["data"]["Catalog"]["searchStore"]["elements"].as_array().cloned().unwrap_or_default();
    let (mut current, mut upcoming): (Vec<Value>, Vec<(DateTime<Utc>, Value)>) = (vec![], vec![]);
    for el in els {
        let promos = &el["promotions"];
        let price = &el["price"]["totalPrice"];
        let op = &price["fmtPrice"]["originalPrice"];
        let base = json!({"title": el["title"].as_str().unwrap_or("").trim(), "url": link(&el),
            "price": if py::truthy(op) { py::str_of(op) } else { String::new() },
            "description": util::oneline(el["description"].as_str().unwrap_or(""), 200)});
        let id = py::str_of(&el["id"]);
        for (a, b) in free_offers(&promos["promotionalOffers"]) {
            let dp = price.get("discountPrice").cloned().unwrap_or(json!(0));
            if a <= now && now < b && dp.as_f64() == Some(0.0) {
                let mut g = base.clone();
                g["key"] = json!(format!("{id}@{}", iso(&a)));
                g["start"] = json!(a.to_rfc3339());
                g["end"] = json!(b.to_rfc3339());
                current.push(g);
            }
        }
        for (a, b) in free_offers(&promos["upcomingPromotionalOffers"]) {
            if a > now {
                let mut g = base.clone();
                g["key"] = json!(format!("{id}@{}", iso(&a)));
                g["start"] = json!(a.to_rfc3339());
                g["end"] = json!(b.to_rfc3339());
                upcoming.push((a, g));
            }
        }
    }
    upcoming.sort_by_key(|x| x.0);
    (current, upcoming.into_iter().map(|x| x.1).collect())
}

fn parse_dt(v: &Value) -> DateTime<Utc> {
    v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now)
}

fn event(g: &Value, upcoming: &[Value]) -> Value {
    let mut lines =
        vec![format!("免费领取：{} 到 {}（过了时间就恢复原价，领到的永久归你）", day(&parse_dt(&g["start"])), day(&parse_dt(&g["end"])))];
    let price = g["price"].as_str().unwrap_or("");
    if !price.is_empty() && price != "0" {
        lines.push(format!("原价：{price}"));
    }
    let desc = g["description"].as_str().unwrap_or("");
    if !desc.is_empty() && desc != g["title"].as_str().unwrap_or("") {
        lines.push(format!("简介：{desc}"));
    }
    if !upcoming.is_empty() {
        lines.push(format!(
            "下一批预告：{}",
            upcoming
                .iter()
                .take(5)
                .map(|u| format!("{}（{} 起）", u["title"].as_str().unwrap_or(""), day(&parse_dt(&u["start"]))))
                .collect::<Vec<_>>()
                .join("；")
        ));
    }
    json!({"id": format!("epic-{}", py::str_of(&g["key"])), "title": format!("Epic 免费送：{}", g["title"].as_str().unwrap_or("")), "text": lines.join("\n"), "url": g["url"]})
}

async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let country = if py::truthy(&params["country"]) { py::str_of(&params["country"]) } else { "CN".into() };
    let r = util::get(API, 20.0, 3_000_000, &[], &[("locale", "zh-CN".into()), ("country", country.clone()), ("allowCountries", country)])
        .await
        .map_err(|e| e.text())?;
    let (current, upcoming) = games(&r.json(), Utc::now());
    let seen: Vec<String> = state["seen"].as_array().map(|a| a.iter().map(py::str_of).collect()).unwrap_or_default();
    let new: Vec<Value> = current.into_iter().filter(|g| !seen.contains(&py::str_of(&g["key"]))).collect();
    let mut all: Vec<String> = new.iter().map(|g| py::str_of(&g["key"])).chain(seen).collect();
    all.truncate(MAX_SEEN);
    Ok((new.iter().map(|g| event(g, &upcoming)).collect(), json!({"seen": all})))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("epic_free");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.summary = Some(|p| {
        let c = if py::truthy(&p["country"]) { py::str_of(&p["country"]) } else { "CN".into() };
        if c == "CN" {
            String::new()
        } else {
            format!("{}商店", COUNTRIES.iter().find(|x| x.0 == c).map(|x| x.1.to_string()).unwrap_or(c))
        }
    });
    vec![s]
}
