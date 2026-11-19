//! 地震提醒：全球或你附近发生地震时报一次。数据来自 USGS，地名用 Open-Meteo 查。对应 osa/sources/earthquake.py。

use super::util;
use crate::py;
use crate::registry::Source;
use chrono::{Datelike, Local, TimeZone};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const FEED: &str = "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/{}_day.geojson";
pub const GEOCODE: &str = "https://geocoding-api.open-meteo.com/v1/search";
const KEEP_MS: i64 = 3 * 86400 * 1000;
static LATLON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(-?\d+(?:\.\d+)?)\s*[,，\s]\s*(-?\d+(?:\.\d+)?)\s*$").unwrap());

fn alert(a: &str) -> Option<&'static str> {
    match a {
        "yellow" => Some("黄色（可能有伤亡和损失）"),
        "orange" => Some("橙色（伤亡和损失可能较大）"),
        "red" => Some("红色（伤亡和损失可能很大）"),
        _ => None,
    }
}

/// 两点之间的大圆距离（公里）
pub fn distance(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let a = ((p2 - p1) / 2.0).sin().powi(2) + p1.cos() * p2.cos() * ((lon2 - lon1).to_radians() / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.min(1.0).sqrt().asin()
}

pub fn latlon(s: &str) -> Result<Option<(f64, f64)>, String> {
    let Some(m) = LATLON.captures(s) else { return Ok(None) };
    let (lat, lon): (f64, f64) = (m[1].parse().unwrap_or(0.0), m[2].parse().unwrap_or(0.0));
    if !((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)) {
        return Err("纬度要在 -90 到 90 之间，经度在 -180 到 180 之间".into());
    }
    Ok(Some((lat, lon)))
}

pub async fn locate(near: &str, cached: &Value) -> Result<Value, String> {
    let near = near.trim();
    if near.is_empty() {
        return Ok(Value::Null);
    }
    if cached.is_object() && cached["q"] == near {
        return Ok(cached.clone());
    }
    if let Some((lat, lon)) = latlon(near)? {
        return Ok(json!({"q": near, "lat": lat, "lon": lon, "name": near, "place": format!("{},{}", util::g(lat, 6), util::g(lon, 6))}));
    }
    let r = util::get(
        GEOCODE,
        20.0,
        3_000_000,
        &[],
        &[("name", near.to_string()), ("count", "5".into()), ("language", "zh".into()), ("format", "json".into())],
    )
    .await
    .map_err(|e| e.text())?;
    let hits = r.json()["results"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return Err(format!("找不到“{near}”在哪：换个写法，试试英文名（Tokyo），或者直接填“纬度,经度”"));
    }
    // max：同样多的取第一个
    let mut h = &hits[0];
    for x in &hits {
        if x["population"].as_f64().unwrap_or(0.0) > h["population"].as_f64().unwrap_or(0.0) {
            h = x;
        }
    }
    let name = if py::truthy(&h["name"]) { py::str_of(&h["name"]) } else { near.to_string() };
    let mut extra: Vec<String> = vec![];
    for x in [&h["country"], &h["admin1"]] {
        if py::truthy(x) {
            let s = py::str_of(x);
            if s != name && !extra.contains(&s) {
                extra.push(s);
            }
        }
    }
    Ok(json!({"q": near, "lat": h["latitude"].as_f64().unwrap_or(0.0), "lon": h["longitude"].as_f64().unwrap_or(0.0), "name": near,
              "place": format!("{name}{}", if extra.is_empty() { String::new() } else { format!("（{}）", extra.join(" ")) })}))
}

fn time_of(ms: i64) -> String {
    let d = Local.timestamp_millis_opt(ms).single().unwrap_or_else(Local::now);
    format!("{}年{}月{}日 {}", d.year(), d.month(), d.day(), d.format("%H:%M"))
}

/// USGS 的 GeoJSON → 符合条件的地震（按时间从早到晚）
pub fn quakes(data: &Value, min_mag: f64, wh: &Value, radius: f64) -> Vec<Value> {
    let mut out = vec![];
    for f in data["features"].as_array().cloned().unwrap_or_default() {
        let p = &f["properties"];
        let coords = f["geometry"]["coordinates"].as_array().cloned().unwrap_or_default();
        let Some(mag) = p["mag"].as_f64() else { continue };
        if p["type"] != "earthquake" || mag < min_mag || coords.len() < 2 || !py::truthy(&f["id"]) {
            continue;
        }
        let mut q = json!({"id": f["id"], "mag": p["mag"], "mag_type": if py::truthy(&p["magType"]) { p["magType"].clone() } else { json!("") },
            "place": if py::truthy(&p["place"]) { p["place"].clone() } else { json!("") }, "time": py::to_int(&p["time"]).unwrap_or(0),
            "lon": coords[0], "lat": coords[1], "depth": coords.get(2).cloned().unwrap_or(Value::Null),
            "url": if py::truthy(&p["url"]) { p["url"].clone() } else { json!("") }, "tsunami": py::truthy(&p["tsunami"]),
            "alert": if py::truthy(&p["alert"]) { p["alert"].clone() } else { json!("") }, "felt": p["felt"]});
        if wh.is_object() {
            let d = distance(wh["lat"].as_f64().unwrap_or(0.0), wh["lon"].as_f64().unwrap_or(0.0), q["lat"].as_f64().unwrap_or(0.0), q["lon"].as_f64().unwrap_or(0.0));
            q["distance"] = json!(d);
            if d > radius {
                continue;
            }
        }
        out.push(q);
    }
    out.sort_by_key(|q| q["time"].as_i64().unwrap_or(0));
    out
}

fn spaced(s: &str) -> String {
    let a = s.chars().next().is_some_and(|c| c.is_ascii());
    let z = s.chars().last().is_some_and(|c| c.is_ascii());
    format!("{}{s}{}", if a { " " } else { "" }, if z { " " } else { "" })
}

pub fn event(q: &Value, wh: &Value) -> Value {
    let mag = util::f(q["mag"].as_f64().unwrap_or(0.0), 1);
    let place = q["place"].as_str().unwrap_or("");
    let dist = q["distance"].as_f64().unwrap_or(0.0);
    let far = if wh.is_object() { format!("，离{}约 {} 公里", spaced(wh["name"].as_str().unwrap_or("")), util::f(dist, 0)) } else { String::new() };
    let mt = q["mag_type"].as_str().unwrap_or("");
    let mut lines = vec![
        format!("震级：M{mag}{}", if mt.is_empty() { String::new() } else { format!("（{mt}）") }),
        format!("地点：{}（{}, {}）", if place.is_empty() { "未知" } else { place }, util::f(q["lat"].as_f64().unwrap_or(0.0), 2), util::f(q["lon"].as_f64().unwrap_or(0.0), 2)),
        format!("时间：{}", time_of(q["time"].as_i64().unwrap_or(0))),
    ];
    if let Some(d) = q["depth"].as_f64() {
        lines.push(format!("深度：{} 公里", util::f(d, 0)));
    }
    if wh.is_object() {
        let at = if py::truthy(&wh["place"]) { py::str_of(&wh["place"]) } else { py::str_of(&wh["name"]) };
        lines.push(format!("离你：约 {} 公里（按“{at}”算）", util::f(dist, 0)));
    }
    if q["tsunami"] == true {
        lines.push("海啸：USGS 标了海啸相关信息，留意当地的海啸预警".into());
    }
    if let Some(a) = alert(q["alert"].as_str().unwrap_or("")) {
        lines.push(format!("影响评估（PAGER）：{a}"));
    }
    if py::truthy(&q["felt"]) {
        lines.push(format!("有感报告：{} 人", py::str_of(&q["felt"])));
    }
    lines.push("数据来自 USGS，刚发生的震级、位置之后可能会修正。".into());
    json!({"id": format!("quake-{}", py::str_of(&q["id"])), "title": format!("M{mag} 地震：{}{far}", if place.is_empty() { "未知地点" } else { place }),
           "text": lines.join("\n"), "url": q["url"]})
}

fn or(p: &Value, k: &str, d: f64) -> f64 {
    if py::truthy(&p[k]) { p[k].as_f64().unwrap_or(d) } else { d }
}

pub async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let state = if state.is_object() { state } else { json!({}) };
    let min_mag = or(&params, "min_magnitude", 5.0);
    let radius = or(&params, "radius_km", 500.0);
    let near = if params["near"].is_string() { py::str_of(&params["near"]) } else { String::new() };
    let wh = locate(&near, &state["where"]).await?;
    let url = FEED.replace("{}", if min_mag >= 4.5 { "4.5" } else { "2.5" });
    let r = util::get(&url, 20.0, 3_000_000, &[], &[]).await.map_err(|e| e.text())?;
    let found = quakes(&r.json(), min_mag, &wh, radius);
    let mut seen = state["seen"].as_object().cloned().unwrap_or_default();
    let new: Vec<Value> = found.iter().filter(|q| !seen.contains_key(&py::str_of(&q["id"]))).cloned().collect();
    let newest = found.iter().filter_map(|q| q["time"].as_i64()).chain(seen.values().filter_map(|v| v.as_i64())).chain([0]).max().unwrap_or(0);
    for q in &new {
        seen.insert(py::str_of(&q["id"]), q["time"].clone());
    }
    seen.retain(|_, t| t.as_i64().unwrap_or(0) > newest - KEEP_MS);
    Ok((new.iter().map(|q| event(q, &wh)).collect(), json!({"seen": seen, "where": wh})))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("earthquake");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.validate = Some(|mut p| {
        if p["min_magnitude"].is_null() {
            p["min_magnitude"] = json!(5);
        }
        if p["radius_km"].is_null() {
            p["radius_km"] = json!(500);
        }
        latlon(p["near"].as_str().unwrap_or(""))?;
        Ok(p)
    });
    s.summary = Some(|p| {
        let mag = format!("{} 级以上", util::g(or(p, "min_magnitude", 5.0), 6));
        let near = p["near"].as_str().unwrap_or("").trim().to_string();
        if !near.is_empty() {
            return format!("{near} {} 公里内、{mag}", util::g(or(p, "radius_km", 500.0), 6));
        }
        format!("全球 {mag}")
    });
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> Value {
        json!({"features": [
            {"id": "us1", "properties": {"type": "earthquake", "mag": 6.1, "magType": "mww", "place": "near Tokyo", "time": 1763600000000i64,
             "url": "https://usgs/us1", "tsunami": 1, "alert": "yellow", "felt": 120}, "geometry": {"coordinates": [139.7, 35.7, 30.0]}},
            {"id": "us2", "properties": {"type": "earthquake", "mag": 4.0, "place": "far", "time": 1763600001000i64}, "geometry": {"coordinates": [0, 0, 5]}},
            {"id": "us3", "properties": {"type": "quarry blast", "mag": 6.5, "time": 1}, "geometry": {"coordinates": [0, 0]}}]})
    }

    #[tokio::test]
    async fn first_then_new() {
        let mock: util::Mock = std::sync::Arc::new(|u: &reqwest::Url| {
            assert!(u.path().ends_with("4.5_day.geojson"));
            (200, feed().to_string().into_bytes())
        });
        let (ev, st) = util::MOCK.scope(mock, async { check(json!({"min_magnitude": 5}), Value::Null).await.unwrap() }).await;
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["title"], "M6.1 地震：near Tokyo");
        let text = ev[0]["text"].as_str().unwrap();
        assert!(text.starts_with("震级：M6.1（mww）\n地点：near Tokyo（35.70, 139.70）\n时间："));
        assert!(text.contains("深度：30 公里\n海啸：USGS 标了海啸相关信息，留意当地的海啸预警\n影响评估（PAGER）：黄色（可能有伤亡和损失）\n有感报告：120 人"));
        assert_eq!(st["seen"], json!({"us1": 1763600000000i64}));
    }

    #[test]
    fn near_and_summary() {
        let wh = json!({"q": "35,139", "lat": 35.0, "lon": 139.0, "name": "35,139", "place": "35,139"});
        let qs = quakes(&feed(), 5.0, &wh, 500.0);
        assert_eq!(qs.len(), 1);
        let ev = event(&qs[0], &wh);
        assert!(ev["title"].as_str().unwrap().starts_with("M6.1 地震：near Tokyo，离 35,139 约 "));
        assert!(latlon("100,1").is_err());
        assert_eq!(latlon(" 30.27，120.15 ").unwrap(), Some((30.27, 120.15)));
    }
}
