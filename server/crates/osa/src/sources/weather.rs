//! 天气提醒：明天下雨、降温、高温、低温、大风、下雪，还有周五报一次周末天气。数据用 Open-Meteo。
//! 对应 osa/sources/weather.py（说明见那里）。

use super::util;
use crate::py;
use crate::registry::Source;
use chrono::{Datelike, NaiveDate, Timelike};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const GEO: &str = "https://geocoding-api.open-meteo.com/v1/search";
pub const FORECAST: &str = "https://api.open-meteo.com/v1/forecast";
const DAILY: [&str; 7] = [
    "weather_code", "temperature_2m_max", "temperature_2m_min", "precipitation_probability_max", "precipitation_sum",
    "wind_speed_10m_max", "snowfall_sum",
];
const SHORT: [&str; 7] = ["code", "tmax", "tmin", "prob", "rain", "wind", "snow"];
const QUIET: (u32, u32) = (22, 7);
const RAIN: [i64; 16] = [51, 53, 55, 56, 57, 61, 63, 65, 66, 67, 80, 81, 82, 95, 96, 99];
const SNOW: [i64; 6] = [71, 73, 75, 77, 85, 86];
const WEEK: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

fn wmo(code: i64) -> Option<&'static str> {
    Some(match code {
        0 => "晴",
        1 => "大致晴朗",
        2 => "多云",
        3 => "阴",
        45 => "雾",
        48 => "雾凇",
        51 => "小毛毛雨",
        53 => "毛毛雨",
        55 => "大毛毛雨",
        56 => "冻毛毛雨",
        57 => "强冻毛毛雨",
        61 => "小雨",
        63 => "中雨",
        65 => "大雨",
        66 => "冻雨",
        67 => "强冻雨",
        71 => "小雪",
        73 => "中雪",
        75 => "大雪",
        77 => "雪粒",
        80 => "小阵雨",
        81 => "阵雨",
        82 => "强阵雨",
        85 => "小阵雪",
        86 => "大阵雪",
        95 => "雷阵雨",
        96 => "雷阵雨伴有冰雹",
        99 => "强雷阵雨伴有冰雹",
        _ => return None,
    })
}

/// 天气代码（JSON 里可能是 61 也可能是 61.0）
fn code_of(v: &Value) -> Option<i64> {
    v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// Python 的 _f：没有是 —；接近整数的不带小数
pub fn fmt(v: &Value) -> String {
    match num(v) {
        None => "—".into(),
        Some(x) if (x - py::round0(x) as f64).abs() < 0.05 => util::f(x, 0),
        Some(x) => util::f(x, 1),
    }
}

fn err(e: util::FetchErr) -> String {
    e.text()
}

/// 城市名 → {"q", "lat", "lon", "name"}。也能直接写经纬度“30.27,120.15”
pub async fn geocode(city: &str) -> Result<Value, String> {
    static LL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(-?\d+(?:\.\d+)?)\s*[,，]\s*(-?\d+(?:\.\d+)?)\s*$").unwrap());
    static SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(市|区|县|省)$").unwrap());
    if let Some(m) = LL.captures(city) {
        let lat: f64 = m[1].parse().unwrap_or(0.0);
        let lon: f64 = m[2].parse().unwrap_or(0.0);
        return Ok(json!({"q": city, "lat": lat, "lon": lon, "name": city.trim()}));
    }
    let mut names = vec![city.trim().to_string()];
    let short = SUFFIX.replace(&names[0], "").into_owned();
    if short != names[0] && !short.is_empty() {
        names.push(short);
    }
    for name in &names {
        let r = util::get(
            GEO,
            15.0,
            200_000,
            &[],
            &[("name", name.clone()), ("count", "1".into()), ("language", "zh".into()), ("format", "json".into())],
        )
        .await
        .map_err(err)?;
        let d = r.json();
        if let Some(g) = d["results"].as_array().and_then(|a| a.first()).cloned() {
            let gname = if g.get("name").is_some() { py::str_of(&g["name"]) } else { name.clone() };
            let wh: Vec<String> = [&g["admin1"], &g["country"]]
                .iter()
                .filter(|x| py::truthy(x) && **x != &g["name"])
                .map(|x| py::str_of(x))
                .collect();
            let wh = wh.join("，");
            return Ok(json!({"q": city, "lat": g["latitude"], "lon": g["longitude"],
                             "name": format!("{gname}{}", if wh.is_empty() { String::new() } else { format!("（{wh}）") })}));
        }
    }
    Err(format!("找不到城市：{city}。换个写法试试，比如“杭州”或“Hangzhou”，也可以直接写经纬度"))
}

pub async fn forecast(lat: &Value, lon: &Value) -> Result<Value, String> {
    let r = util::get(
        FORECAST,
        15.0,
        500_000,
        &[],
        &[
            ("latitude", py::str_of(lat)),
            ("longitude", py::str_of(lon)),
            ("daily", DAILY.join(",")),
            ("timezone", "auto".into()),
            ("forecast_days", "4".into()),
        ],
    )
    .await
    .map_err(err)?;
    let d = r.json();
    let d = if d.is_null() { json!({}) } else { d };
    if !py::truthy(&d["daily"]["time"]) {
        let why = if py::truthy(&d["reason"]) { py::str_of(&d["reason"]) } else { py::repr(&d) };
        return Err(format!("Open-Meteo 没返回预报：{why}"));
    }
    Ok(d)
}

/// 每天一个对象：date、code、tmax、tmin、prob、rain、wind、snow
pub fn days_of(data: &Value) -> Vec<Value> {
    let d = &data["daily"];
    let mut out = vec![];
    for (i, day) in d["time"].as_array().cloned().unwrap_or_default().into_iter().enumerate() {
        let mut o = json!({"date": day});
        for (k, s) in DAILY.iter().zip(SHORT.iter()) {
            o[*s] = d[*k].as_array().and_then(|a| a.get(i)).cloned().unwrap_or(Value::Null);
        }
        out.push(o);
    }
    out
}

pub fn describe_day(label: &str, d: &Value) -> String {
    let dt = NaiveDate::parse_from_str(d["date"].as_str().unwrap_or(""), "%Y-%m-%d").unwrap_or_default();
    let code = &d["code"];
    let mut parts = vec![
        if code.is_null() {
            String::new()
        } else {
            code_of(code).and_then(wmo).map(String::from).unwrap_or_else(|| format!("天气代码 {}", py::str_of(code)))
        },
        format!("{}~{}°C", fmt(&d["tmin"]), fmt(&d["tmax"])),
    ];
    if !d["prob"].is_null() {
        parts.push(format!("降水概率 {}%", fmt(&d["prob"])));
    }
    if py::truthy(&d["rain"]) {
        parts.push(format!("降水 {} mm", fmt(&d["rain"])));
    }
    if py::truthy(&d["snow"]) {
        parts.push(format!("降雪 {} cm", fmt(&d["snow"])));
    }
    if !d["wind"].is_null() {
        parts.push(format!("最大风速 {} km/h", fmt(&d["wind"])));
    }
    let parts: Vec<String> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    format!("{label} {}月{}日 周{}：{}", dt.month(), dt.day(), WEEK[dt.weekday().num_days_from_monday() as usize], parts.join("，"))
}

/// params.get(k) or default（0 和没给都算没给）
fn or(p: &Value, k: &str, d: f64) -> f64 {
    if py::truthy(&p[k]) { num(&p[k]).unwrap_or(d) } else { d }
}

fn rainy(d: &Value, prob: f64) -> bool {
    if let Some(p) = num(&d["prob"]) {
        return p >= prob;
    }
    num(&d["rain"]).unwrap_or(0.0) >= 0.5 || code_of(&d["code"]).is_some_and(|c| RAIN.contains(&c))
}

/// 哪些情况成立：[(“情况:日期”, 一句说明)]
pub fn hits(params: &Value, days: &[Value], today: &str, weekday: u32) -> Vec<(String, String)> {
    let Some(i) = days.iter().position(|d| d["date"] == today) else { return vec![] };
    if i + 1 >= days.len() {
        return vec![];
    }
    let (t, m) = (&days[i], &days[i + 1]);
    let mdate = py::str_of(&m["date"]);
    let mcode = code_of(&m["code"]);
    let mut out = vec![];
    for c in params["conditions"].as_array().cloned().unwrap_or_default() {
        let c = py::str_of(&c);
        let key = format!("{c}:{mdate}");
        match c.as_str() {
            "rain_tomorrow" if rainy(m, or(params, "rain_prob", 50.0)) => {
                let what = match mcode {
                    Some(code) if RAIN.contains(&code) => wmo(code).unwrap_or("有雨").to_string(),
                    _ => "可能有雨".into(),
                };
                let prob = if m["prob"].is_null() { String::new() } else { format!("（降水概率 {}%）", fmt(&m["prob"])) };
                out.push((key, format!("明天{what}{prob}")));
            }
            "temp_drop" => {
                if let (Some(a), Some(b), Some(c2), Some(d2)) = (num(&t["tmax"]), num(&m["tmax"]), num(&t["tmin"]), num(&m["tmin"])) {
                    let drop = (a - b).max(c2 - d2);
                    if drop >= or(params, "drop", 6.0) {
                        out.push((key, format!("明天降温 {}°C（{}~{}°C）", fmt(&json!(drop)), fmt(&m["tmin"]), fmt(&m["tmax"]))));
                    }
                }
            }
            "hot" => {
                let line = if params["hot"].is_null() { 35.0 } else { num(&params["hot"]).unwrap_or(35.0) };
                if num(&m["tmax"]).is_some_and(|x| x >= line) {
                    out.push((key, format!("明天高温 {}°C", fmt(&m["tmax"]))));
                }
            }
            "cold" => {
                let line = if params["cold"].is_null() { 0.0 } else { num(&params["cold"]).unwrap_or(0.0) };
                if num(&m["tmin"]).is_some_and(|x| x <= line) {
                    out.push((key, format!("明天最低 {}°C", fmt(&m["tmin"]))));
                }
            }
            "wind" if num(&m["wind"]).is_some_and(|x| x >= or(params, "wind", 50.0)) => {
                out.push((key, format!("明天大风，最大风速 {} km/h", fmt(&m["wind"]))));
            }
            "snow" if num(&m["snow"]).unwrap_or(0.0) > 0.0 || mcode.is_some_and(|c| SNOW.contains(&c)) => {
                let what = match mcode {
                    Some(code) if SNOW.contains(&code) => wmo(code).unwrap_or("有雪").to_string(),
                    _ => "可能下雪".into(),
                };
                let amount = if py::truthy(&m["snow"]) { format!("（{} cm）", fmt(&m["snow"])) } else { String::new() };
                out.push((key, format!("明天{what}{amount}")));
            }
            "weekend" if weekday == 4 => out.push((format!("weekend:{mdate}"), "周末天气".into())),
            _ => {}
        }
    }
    out
}

pub fn judge(params: &Value, geo: &Value, data: &Value, state: &Value, now: f64) -> (Vec<Value>, Value) {
    let mut st = if state.is_object() { state.clone() } else { json!({}) };
    let off = py::to_int(&data["utc_offset_seconds"]).unwrap_or(0);
    let local = chrono::DateTime::from_timestamp((now + off as f64).floor() as i64, 0).unwrap_or_default().naive_utc();
    let today = local.format("%Y-%m-%d").to_string();
    let fired: Vec<String> = st["fired"]
        .as_array()
        .map(|a| a.iter().map(py::str_of).filter(|k| k.split_once(':').map(|x| x.1).unwrap_or(k) >= today.as_str()).collect())
        .unwrap_or_default();
    st["fired"] = json!(fired);
    let hour = local.hour();
    let new: Vec<(String, String)> = if QUIET.1 <= hour && hour < QUIET.0 {
        hits(params, &days_of(data), &today, local.weekday().num_days_from_monday()).into_iter().filter(|(k, _)| !fired.contains(k)).collect()
    } else {
        vec![]
    };
    if new.is_empty() {
        return (vec![], st);
    }
    let mut all = fired.clone();
    all.extend(new.iter().map(|(k, _)| k.clone()));
    st["fired"] = json!(all);
    let days = days_of(data);
    let i = days.iter().position(|d| d["date"] == today.as_str()).unwrap_or(0);
    let lines: Vec<String> = ["今天", "明天", "后天"].iter().zip(days[i..].iter().take(3)).map(|(lb, d)| describe_day(lb, d)).collect();
    let name = py::str_of(&geo["name"]);
    let ids: Vec<&str> = new.iter().map(|(k, _)| k.as_str()).collect();
    let says: Vec<&str> = new.iter().map(|(_, s)| s.as_str()).collect();
    (vec![json!({"id": format!("weather-{name}-{}", ids.join("-")), "title": format!("{name}：{}", says.join("；")), "text": lines.join("\n")})], st)
}

pub async fn check_at(params: Value, state: Value, now: f64) -> Result<(Vec<Value>, Value), String> {
    let mut st = if state.is_object() { state.clone() } else { json!({}) };
    let city = params["city"].as_str().unwrap_or("").trim().to_string();
    let geo = if st["geo"]["q"] == city.as_str() { st["geo"].clone() } else { geocode(&city).await? };
    st["geo"] = geo.clone();
    let data = forecast(&geo["lat"], &geo["lon"]).await?;
    Ok(judge(&params, &geo, &data, &st, now))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("weather");
    s.check = Some(|p, st| Box::pin(check_at(p, st, py::now())));
    s.validate = Some(|p| {
        if !py::truthy(&p["conditions"]) {
            return Err("至少选一种要提醒的情况".into());
        }
        Ok(p)
    });
    s.summary = Some(|p| if py::truthy(&p["city"]) { py::str_of(&p["city"]) } else { String::new() });
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn at(y: i32, mo: u32, d: u32, h: u32) -> f64 {
        FixedOffset::east_opt(8 * 3600).unwrap().with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap().timestamp() as f64
    }

    fn forecast(cols: Value) -> Value {
        let mut daily = json!({"time": ["2026-11-09", "2026-11-10", "2026-11-11", "2026-11-12"],
            "weather_code": [0, 0, 0, 0], "temperature_2m_max": [20, 20, 20, 20], "temperature_2m_min": [12, 12, 12, 12],
            "precipitation_probability_max": [0, 0, 0, 0], "precipitation_sum": [0, 0, 0, 0],
            "wind_speed_10m_max": [10, 10, 10, 10], "snowfall_sum": [0, 0, 0, 0]});
        for (k, v) in cols.as_object().unwrap() {
            daily[k] = v.clone();
        }
        json!({"utc_offset_seconds": 28800, "timezone": "Asia/Shanghai", "daily": daily})
    }

    fn geo() -> Value {
        json!({"q": "杭州", "lat": 30.29, "lon": 120.16, "name": "杭州（浙江，中国）"})
    }

    #[test]
    fn rain_tomorrow_once() {
        let p = json!({"city": "杭州", "conditions": ["rain_tomorrow"], "rain_prob": 50});
        let fc = forecast(json!({"weather_code": [3, 63, 3, 0], "precipitation_probability_max": [10, 85, 30, 0], "precipitation_sum": [0, 12.4, 0, 0]}));
        let (ev, st) = judge(&p, &geo(), &fc, &json!({}), at(2026, 11, 9, 9));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["title"], "杭州（浙江，中国）：明天中雨（降水概率 85%）");
        assert_eq!(ev[0]["id"], "weather-杭州（浙江，中国）-rain_tomorrow:2026-11-10");
        assert!(ev[0]["text"].as_str().unwrap().starts_with("今天 11月9日 周一：阴，12~20°C，降水概率 10%，最大风速 10 km/h\n明天 11月10日 周二：中雨，12~20°C，降水概率 85%，降水 12.4 mm"));
        let (again, _) = judge(&p, &geo(), &fc, &st, at(2026, 11, 9, 12));
        assert!(again.is_empty());
        // 夜里不报
        let (night, _) = judge(&p, &geo(), &fc, &json!({}), at(2026, 11, 9, 23));
        assert!(night.is_empty());
    }

    #[test]
    fn drop_hot_cold_weekend() {
        let p = json!({"conditions": ["temp_drop", "hot", "cold", "wind", "snow", "weekend"], "drop": 6});
        let fc = forecast(json!({"temperature_2m_max": [19, 12, 13, 13], "temperature_2m_min": [13, 8, 7, 7]}));
        // 2026-11-13 是周五
        let fc2 = forecast(json!({"time": ["2026-11-13", "2026-11-14", "2026-11-15", "2026-11-16"]}));
        let h = hits(&p, &days_of(&fc), "2026-11-09", 0);
        assert_eq!(h, vec![("temp_drop:2026-11-10".to_string(), "明天降温 7°C（8~12°C）".to_string())]);
        let h = hits(&p, &days_of(&fc2), "2026-11-13", 4);
        assert_eq!(h, vec![("weekend:2026-11-14".to_string(), "周末天气".to_string())]);
        assert_eq!(fmt(&json!(12.4)), "12.4");
        assert_eq!(fmt(&json!(12.04)), "12");
        assert_eq!(fmt(&Value::Null), "—");
    }

    #[tokio::test]
    async fn geocode_and_cache() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let s2 = seen.clone();
        let mock: util::Mock = std::sync::Arc::new(move |u: &reqwest::Url| {
            s2.lock().unwrap().push(u.to_string());
            if u.host_str() == Some("geocoding-api.open-meteo.com") {
                let q: String = u.query_pairs().find(|(k, _)| k == "name").map(|x| x.1.into_owned()).unwrap_or_default();
                let body = if q == "杭州" {
                    json!({"results": [{"name": "杭州", "latitude": 30.29, "longitude": 120.16, "country": "中国", "admin1": "浙江"}]})
                } else {
                    json!({})
                };
                return (200, body.to_string().into_bytes());
            }
            (200, forecast(json!({})).to_string().into_bytes())
        });
        let (ev, st) = util::MOCK
            .scope(mock, async { check_at(json!({"city": "杭州市", "conditions": ["rain_tomorrow"]}), Value::Null, at(2026, 11, 9, 9)).await.unwrap() })
            .await;
        assert!(ev.is_empty());
        assert_eq!(st["geo"], json!({"q": "杭州市", "lat": 30.29, "lon": 120.16, "name": "杭州（浙江，中国）"}));
        let first = seen.lock().unwrap().clone();
        assert_eq!(first.len(), 3);   // 杭州市 查不到，再查 杭州，再取预报
        assert!(first[2].contains("latitude=30.29") && first[2].contains("forecast_days=4"));
    }
}
