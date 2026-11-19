//! 日程快开始了：订阅 ICS 日历地址，开始前几分钟提醒一次。重复的按 RRULE 展开，去掉 EXDATE、取消了的、单独改过的那一次。
//! 对应 osa/sources/calendar.py（说明见那里）。

use super::util::{self, oneline, short};
use crate::registry::Source;
use crate::{py, vault};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc};
use regex::Regex;
use serde_json::{Value, json};
use sha1::Digest;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

pub const FETCH_EVERY: f64 = 30.0 * 60.0;
pub const STALE: f64 = 3.0 * 3600.0;
pub const HORIZON: f64 = 2.0 * 86400.0;
pub const MAX_ITEMS: usize = 100;
pub const MAX_FIRED: usize = 300;
pub const ALL_DAY_AT: f64 = 9.0;
const WEEK: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

const WINDOWS: [(&str, &str); 35] = [
    ("China Standard Time", "Asia/Shanghai"), ("Taipei Standard Time", "Asia/Taipei"), ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Korea Standard Time", "Asia/Seoul"), ("Singapore Standard Time", "Asia/Singapore"), ("India Standard Time", "Asia/Kolkata"),
    ("SE Asia Standard Time", "Asia/Bangkok"), ("Arabian Standard Time", "Asia/Dubai"), ("Russian Standard Time", "Europe/Moscow"),
    ("GMT Standard Time", "Europe/London"), ("Greenwich Standard Time", "Atlantic/Reykjavik"), ("W. Europe Standard Time", "Europe/Berlin"),
    ("Romance Standard Time", "Europe/Paris"), ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"), ("E. Europe Standard Time", "Europe/Chisinau"),
    ("FLE Standard Time", "Europe/Kiev"), ("GTB Standard Time", "Europe/Bucharest"), ("Eastern Standard Time", "America/New_York"),
    ("Central Standard Time", "America/Chicago"), ("Mountain Standard Time", "America/Denver"), ("US Mountain Standard Time", "America/Phoenix"),
    ("Pacific Standard Time", "America/Los_Angeles"), ("Alaskan Standard Time", "America/Anchorage"), ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("Atlantic Standard Time", "America/Halifax"), ("E. South America Standard Time", "America/Sao_Paulo"),
    ("AUS Eastern Standard Time", "Australia/Sydney"), ("E. Australia Standard Time", "Australia/Brisbane"),
    ("W. Australia Standard Time", "Australia/Perth"), ("New Zealand Standard Time", "Pacific/Auckland"), ("UTC", "UTC"),
    ("Coordinated Universal Time", "UTC"), ("", ""), ("", ""),
];

// ---------- 读 ICS ----------
type Prop = (HashMap<String, String>, String);
pub type Event = HashMap<String, Vec<Prop>>;

pub struct Cal {
    pub tz: String,
    pub name: String,
    pub vtz: HashMap<String, i64>,
    pub events: Vec<Event>,
}

fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for ln in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        if (ln.starts_with(' ') || ln.starts_with('\t')) && !out.is_empty() {
            let rest: String = ln.chars().skip(1).collect();
            out.last_mut().unwrap().push_str(&rest);
        } else if !ln.trim().is_empty() {
            out.push(ln.to_string());
        }
    }
    out
}

fn split(ln: &str) -> (String, HashMap<String, String>, String) {
    let mut quoted = false;
    let mut start = 0;
    let mut parts: Vec<&str> = vec![];
    for (i, ch) in ln.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        } else if !quoted && ch == ';' {
            parts.push(&ln[start..i]);
            start = i + 1;
        } else if !quoted && ch == ':' {
            parts.push(&ln[start..i]);
            let mut params = HashMap::new();
            for p in &parts[1..] {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                params.insert(k.trim().to_uppercase(), v.trim().trim_matches('"').to_string());
            }
            return (parts[0].trim().to_uppercase(), params, ln[i + 1..].to_string());
        }
    }
    (String::new(), HashMap::new(), String::new())
}

fn text(v: &str) -> String {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\([nN,;\\])").unwrap());
    R.replace_all(v, |c: &regex::Captures| if &c[1] == "n" || &c[1] == "N" { "\n".to_string() } else { c[1].to_string() }).trim().to_string()
}

fn offset(v: &str) -> i64 {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([+-])(\d{2})(\d{2})(\d{2})?$").unwrap());
    let Some(m) = R.captures(v.trim()) else { return 0 };
    let s = m[2].parse::<i64>().unwrap_or(0) * 3600 + m[3].parse::<i64>().unwrap_or(0) * 60 + m.get(4).map(|x| x.as_str().parse::<i64>().unwrap_or(0)).unwrap_or(0);
    if &m[1] == "-" { -s } else { s }
}

pub fn parse(text_: &str) -> Cal {
    let mut cal = Cal { tz: String::new(), name: String::new(), vtz: HashMap::new(), events: vec![] };
    let mut stack: Vec<String> = vec![];
    let mut ev: Option<Event> = None;
    let mut tzid = String::new();
    for ln in unfold(text_) {
        let (name, params, value) = split(&ln);
        let top = stack.last().cloned().unwrap_or_default();
        if name == "BEGIN" {
            stack.push(value.trim().to_uppercase());
            if stack.last().map(|s| s.as_str()) == Some("VEVENT") {
                ev = Some(HashMap::new());
            }
        } else if name == "END" {
            let top = stack.pop().unwrap_or_default();
            if top == "VEVENT" {
                if let Some(e) = ev.take() {
                    cal.events.push(e);
                }
            } else if top == "VTIMEZONE" {
                tzid.clear();
            }
        } else if top == "VEVENT" && ev.is_some() {
            ev.as_mut().unwrap().entry(name).or_default().push((params, value));
        } else if top == "VCALENDAR" && (name == "X-WR-TIMEZONE" || name == "X-WR-CALNAME") {
            if name == "X-WR-TIMEZONE" {
                cal.tz = value.trim().to_string();
            } else {
                cal.name = value.trim().to_string();
            }
        } else if top == "VTIMEZONE" && name == "TZID" {
            tzid = value.trim().to_string();
        } else if top == "STANDARD" && name == "TZOFFSETTO" && !tzid.is_empty() {
            cal.vtz.entry(tzid.clone()).or_insert_with(|| offset(&value));
        }
    }
    cal
}

// ---------- 时间 ----------
/// 时区：IANA、固定偏移，或者这台机器的时区（Python 的 None）
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zone {
    Local,
    Iana(chrono_tz::Tz),
    Fixed(i32),
}

pub fn zone(tzid: &str, vtz: Option<&HashMap<String, i64>>) -> Option<Zone> {
    static PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z]+/[A-Za-z_\-]+(?:/[A-Za-z_\-]+)?").unwrap());
    static UTC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:UTC|GMT)\s*([+-])(\d{1,2})(?::?(\d{2}))?").unwrap());
    let tzid = tzid.trim().trim_matches('"');
    if tzid.is_empty() {
        return None;
    }
    let win = WINDOWS.iter().find(|(k, _)| !k.is_empty() && *k == tzid).map(|(_, v)| *v);
    let mut names: Vec<&str> = vec![tzid];
    names.extend(win);
    names.extend(PATH.find_iter(tzid).map(|m| m.as_str()));
    for n in names {
        if !n.is_empty() {
            if let Ok(z) = n.parse::<chrono_tz::Tz>() {
                return Some(Zone::Iana(z));
            }
        }
    }
    if let Some(m) = UTC.captures(tzid) {
        let s = m[2].parse::<i32>().unwrap_or(0) * 3600 + m.get(3).map(|x| x.as_str().parse::<i32>().unwrap_or(0)).unwrap_or(0) * 60;
        return Some(Zone::Fixed(if &m[1] == "-" { -s } else { s }));
    }
    vtz.and_then(|v| v.get(tzid)).map(|s| Zone::Fixed(*s as i32))
}

/// 墙上时间：全天的是日期
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub enum Wall {
    Date(NaiveDate),
    Time(NaiveDateTime),
}

impl Wall {
    fn naive(&self) -> NaiveDateTime {
        match self {
            Wall::Date(d) => d.and_time(NaiveTime::MIN),
            Wall::Time(t) => *t,
        }
    }
    fn date(&self) -> NaiveDate {
        match self {
            Wall::Date(d) => *d,
            Wall::Time(t) => t.date(),
        }
    }
    fn weekday(&self) -> u32 {
        self.date().weekday().num_days_from_monday()
    }
    fn add_days(&self, n: i64) -> Wall {
        match self {
            Wall::Date(d) => Wall::Date(*d + Duration::days(n)),
            Wall::Time(t) => Wall::Time(*t + Duration::days(n)),
        }
    }
    fn with_ymd(&self, y: i32, m: u32, d: u32) -> Option<Wall> {
        let nd = NaiveDate::from_ymd_opt(y, m, d)?;
        Some(match self {
            Wall::Date(_) => Wall::Date(nd),
            Wall::Time(t) => Wall::Time(nd.and_time(t.time())),
        })
    }
}

fn when(value: &str, params: &HashMap<String, String>, vtz: &HashMap<String, i64>, default: Zone) -> Result<(Wall, Zone), String> {
    static D8: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{8}$").unwrap());
    static DT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})?(Z?)$").unwrap());
    let v = value.trim();
    let bad = || format!("看不懂的时间：{v}");
    if params.get("VALUE").is_some_and(|x| x.to_uppercase() == "DATE") || D8.is_match(v) {
        let d = (|| NaiveDate::from_ymd_opt(v.get(..4)?.parse().ok()?, v.get(4..6)?.parse().ok()?, v.get(6..8)?.parse().ok()?))().ok_or_else(bad)?;
        return Ok((Wall::Date(d), default));
    }
    let m = DT.captures(v).ok_or_else(bad)?;
    let p = |i: usize| m.get(i).map(|x| x.as_str().parse::<u32>().unwrap_or(0)).unwrap_or(0);
    let dt = NaiveDate::from_ymd_opt(m[1].parse().unwrap_or(0), p(2), p(3)).and_then(|d| d.and_hms_opt(p(4), p(5), p(6))).ok_or_else(bad)?;
    if !m[7].is_empty() {
        return Ok((Wall::Time(dt), Zone::Fixed(0)));
    }
    if let Some(z) = params.get("TZID").filter(|t| !t.is_empty()).and_then(|t| zone(t, Some(vtz))) {
        return Ok((Wall::Time(dt), z));
    }
    Ok((Wall::Time(dt), default))
}

/// 墙上时间 → epoch。夏令时重叠的取前一个，跳过的那一段按跳之前的偏移（和 Python 的 fold=0 一样）
fn local_ts<T: TimeZone>(tz: &T, naive: NaiveDateTime) -> f64 {
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.timestamp() as f64,
        chrono::LocalResult::None => {
            let before = tz.from_local_datetime(&(naive - Duration::hours(3))).earliest();
            match before {
                Some(b) => (naive - Duration::seconds(b.offset().fix().local_minus_utc() as i64)).and_utc().timestamp() as f64,
                None => naive.and_utc().timestamp() as f64,
            }
        }
    }
}

pub fn epoch(x: Wall, tz: Zone) -> f64 {
    let n = x.naive();
    match tz {
        Zone::Local => local_ts(&chrono::Local, n),
        Zone::Iana(z) => local_ts(&z, n),
        Zone::Fixed(s) => (n - Duration::seconds(s as i64)).and_utc().timestamp() as f64,
    }
}

/// epoch → 那个时区的墙上时间（带星期）
fn local(ts: f64, tz: Zone) -> NaiveDateTime {
    let u = DateTime::<Utc>::from_timestamp(ts.floor() as i64, ((ts.fract()) * 1e9) as u32).unwrap_or_default();
    match tz {
        Zone::Local => u.with_timezone(&chrono::Local).naive_local(),
        Zone::Iana(z) => u.with_timezone(&z).naive_local(),
        Zone::Fixed(s) => u.naive_utc() + Duration::seconds(s as i64),
    }
}

fn duration(v: &str) -> Option<f64> {
    static R: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^([+-])?P(?:(\d+)W)?(?:(\d+)D)?(?:T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?)?$").unwrap());
    let m = R.captures(v.trim())?;
    let g = |i: usize| m.get(i).map(|x| x.as_str().parse::<f64>().unwrap_or(0.0)).unwrap_or(0.0);
    let s = g(2) * 7.0 * 86400.0 + g(3) * 86400.0 + g(4) * 3600.0 + g(5) * 60.0 + g(6);
    Some(if m.get(1).is_some_and(|x| x.as_str() == "-") { -s } else { s })
}

// ---------- 重复 ----------
fn byday(s: &str) -> Vec<(Option<i64>, u32)> {
    static R: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([+-]?\d{1,2})?(MO|TU|WE|TH|FR|SA|SU)$").unwrap());
    let days = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
    s.split(',')
        .filter_map(|x| {
            let up = x.trim().to_uppercase();
            let m = R.captures(&up)?;
            Some((m.get(1).and_then(|n| n.as_str().parse().ok()), days.iter().position(|d| *d == &m[2])? as u32))
        })
        .collect()
}

fn month_len(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (NaiveDate::from_ymd_opt(ny, nm, 1).unwrap() - NaiveDate::from_ymd_opt(y, m, 1).unwrap()).num_days() as u32
}

fn period(start: Wall, freq: &str, k: i64, byday: &[(Option<i64>, u32)], bymd: &[i64]) -> Vec<Wall> {
    match freq {
        "DAILY" => vec![start.add_days(k)],
        "WEEKLY" => {
            let week = start.add_days(-(start.weekday() as i64) + 7 * k);
            let mut wds: Vec<u32> = byday.iter().map(|(_, w)| *w).collect();
            if wds.is_empty() {
                wds.push(start.weekday());
            }
            wds.sort();
            wds.dedup();
            wds.into_iter().map(|wd| week.add_days(wd as i64)).collect()
        }
        "MONTHLY" => {
            let d0 = start.date();
            let m = d0.month0() as i64 + k;
            let (y, mo) = (d0.year() + m.div_euclid(12) as i32, m.rem_euclid(12) as u32 + 1);
            let last = month_len(y, mo);
            let mut days: Vec<i64> = vec![];
            if !byday.is_empty() {
                for (n, wd) in byday {
                    let same: Vec<i64> = (1..=last).filter(|d| NaiveDate::from_ymd_opt(y, mo, *d).unwrap().weekday().num_days_from_monday() == *wd).map(|d| d as i64).collect();
                    match n {
                        None => days.extend(same),
                        Some(n) if *n != 0 && n.unsigned_abs() as usize <= same.len() => {
                            days.push(if *n > 0 { same[(*n - 1) as usize] } else { same[same.len() - n.unsigned_abs() as usize] });
                        }
                        _ => {}
                    }
                }
            } else if !bymd.is_empty() {
                days = bymd.iter().map(|d| if *d > 0 { *d } else { last as i64 + 1 + d }).collect();
            } else {
                days.push(d0.day() as i64);
            }
            days.sort();
            days.dedup();
            days.into_iter().filter(|d| *d >= 1 && *d <= last as i64).filter_map(|d| start.with_ymd(y, mo, d as u32)).collect()
        }
        "YEARLY" => {
            let d0 = start.date();
            start.with_ymd(d0.year() + k as i32, d0.month(), d0.day()).into_iter().collect()
        }
        _ => vec![],
    }
}

pub fn expand(start: Wall, rule: &HashMap<String, String>, lo: f64, hi: f64, tz: Zone, until: Option<f64>) -> Vec<(Wall, f64)> {
    let freq = rule.get("FREQ").map(|s| s.to_uppercase()).unwrap_or_default();
    if !["DAILY", "WEEKLY", "MONTHLY", "YEARLY"].contains(&freq.as_str()) {
        let ep = epoch(start, tz);
        return if lo <= ep && ep <= hi { vec![(start, ep)] } else { vec![] };
    }
    let step = rule.get("INTERVAL").filter(|s| !s.is_empty()).map(|s| s.parse::<i64>().map(|x| x.max(1)).unwrap_or(1)).unwrap_or(1);
    let count = rule.get("COUNT").filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())).and_then(|s| s.parse::<i64>().ok());
    let bd = byday(rule.get("BYDAY").map(|s| s.as_str()).unwrap_or(""));
    static MD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d{1,2}$").unwrap());
    let bymd: Vec<i64> = rule.get("BYMONTHDAY").map(|s| s.split(',').filter(|x| MD.is_match(x.trim())).filter_map(|x| x.trim().parse().ok()).collect()).unwrap_or_default();
    let s0 = epoch(start, tz);
    let mut k0 = 0i64;
    if count.is_none() && lo > s0 {
        let per = match freq.as_str() {
            "DAILY" => 1,
            "WEEKLY" => 7,
            "MONTHLY" => 31,
            _ => 366,
        } * step;
        k0 = (((lo - s0) / 86400.0 / per as f64).floor() as i64 - 1).max(0);
    }
    let (mut out, mut n) = (vec![], 0i64);
    'outer: for k in k0..k0 + 5000 {
        for c in period(start, &freq, k * step, &bd, &bymd) {
            if c < start {
                continue;
            }
            let ep = epoch(c, tz);
            n += 1;
            if until.is_some_and(|u| ep > u) || count.is_some_and(|cn| n > cn) || ep > hi {
                break 'outer;
            }
            if ep >= lo {
                out.push((c, ep));
            }
        }
    }
    out
}

// ---------- 展开成接下来的日程 ----------
fn first(ev: &Event, key: &str) -> String {
    ev.get(key).and_then(|v| v.first()).map(|p| p.1.clone()).unwrap_or_default()
}

fn person(params: &HashMap<String, String>, value: &str) -> String {
    static MAILTO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^mailto:").unwrap());
    let mail = MAILTO.replace(value, "").trim().to_string();
    let cn = params.get("CN").map(|s| s.trim().to_string()).unwrap_or_default();
    if !cn.is_empty() && !mail.is_empty() && cn != mail {
        format!("{cn} <{mail}>")
    } else if !cn.is_empty() {
        cn
    } else {
        mail
    }
}

pub fn occurrences(cal: &Cal, now: f64, default_local: Zone) -> Vec<Value> {
    let default = zone(&cal.tz, None).unwrap_or(default_local);
    let vtz = &cal.vtz;
    let mut moved: HashMap<String, HashSet<i64>> = HashMap::new();
    for ev in &cal.events {
        if let Some((p, v)) = ev.get("RECURRENCE-ID").and_then(|x| x.first()) {
            if let Ok((x, z)) = when(v, p, vtz, default) {
                moved.entry(first(ev, "UID").trim().to_string()).or_default().insert(epoch(x, z) as i64);
            }
        }
    }
    let mut items: Vec<Value> = vec![];
    for ev in &cal.events {
        if first(ev, "STATUS").trim().to_uppercase() == "CANCELLED" || !ev.get("DTSTART").is_some_and(|v| !v.is_empty()) {
            continue;
        }
        let (p, v) = &ev["DTSTART"][0];
        let Ok((start, tz)) = when(v, p, vtz, default) else { continue };
        let all_day = matches!(start, Wall::Date(_));
        let s0 = epoch(start, tz);
        let mut dur: Option<f64> = None;
        if let Some((ep, evv)) = ev.get("DTEND").and_then(|x| x.first()) {
            if let Ok((end, etz)) = when(evv, ep, vtz, default) {
                dur = Some(epoch(end, etz) - s0);
            }
        }
        if dur.is_none() && ev.get("DURATION").is_some_and(|v| !v.is_empty()) {
            dur = duration(&first(ev, "DURATION"));
        }
        let dur = dur.unwrap_or(if all_day { 86400.0 } else { 0.0 }).max(0.0);
        let uid = {
            let u = first(ev, "UID").trim().to_string();
            if u.is_empty() { format!("{}|{v}", first(ev, "SUMMARY")) } else { u }
        };
        let (lo, hi) = (now - dur, now + HORIZON);
        let occ: Vec<(Wall, f64)> = if ev.get("RRULE").is_some_and(|v| !v.is_empty()) && !ev.get("RECURRENCE-ID").is_some_and(|v| !v.is_empty()) {
            let mut rule = HashMap::new();
            for part in ev["RRULE"][0].1.split(';') {
                let (k, val) = part.split_once('=').unwrap_or((part, ""));
                rule.insert(k.trim().to_uppercase(), val.trim().to_string());
            }
            let mut until = None;
            if let Some(u) = rule.get("UNTIL").filter(|u| !u.is_empty()) {
                if let Ok((u, utz)) = when(u, &HashMap::new(), vtz, tz) {
                    until = Some(match u {
                        Wall::Date(_) => epoch(u.add_days(1), utz) - 1.0,
                        Wall::Time(_) => epoch(u, utz),
                    });
                }
            }
            let mut skip_ep: HashSet<i64> = moved.get(&uid).cloned().unwrap_or_default();
            let mut skip_day: HashSet<NaiveDate> = HashSet::new();
            for (xp, xv) in ev.get("EXDATE").cloned().unwrap_or_default() {
                for x in xv.split(',') {
                    let Ok((xd, xz)) = when(x, &xp, vtz, default) else { continue };
                    match xd {
                        Wall::Time(_) => {
                            skip_ep.insert(epoch(xd, xz) as i64);
                        }
                        Wall::Date(d) => {
                            skip_day.insert(d);
                        }
                    }
                }
            }
            expand(start, &rule, lo, hi, tz, until).into_iter().filter(|(c, e)| !skip_ep.contains(&(*e as i64)) && !skip_day.contains(&c.date())).collect()
        } else if lo <= s0 && s0 <= hi {
            vec![(start, s0)]
        } else {
            vec![]
        };
        if occ.is_empty() {
            continue;
        }
        let people: Vec<String> = ev.get("ATTENDEE").map(|a| a.iter().take(15).map(|(p, v)| person(p, v)).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
        let org = ev.get("ORGANIZER").and_then(|a| a.first()).map(|(p, v)| person(p, v)).unwrap_or_default();
        let title = oneline(&text(&first(ev, "SUMMARY")), 200);
        let url = {
            let u = first(ev, "URL").trim().to_string();
            if u.is_empty() { first(ev, "X-GOOGLE-CONFERENCE").trim().to_string() } else { u }
        };
        for (_, e) in occ {
            items.push(json!({"k": format!("{uid}|{}", e as i64), "s": e, "e": e + dur, "d": all_day,
                "t": if title.is_empty() { "（没有标题的日程）".to_string() } else { title.clone() },
                "l": oneline(&text(&first(ev, "LOCATION")), 300), "x": short(&text(&first(ev, "DESCRIPTION")), 600),
                "u": url, "p": people.join("、"), "o": org}));
        }
    }
    items.sort_by(|a, b| a["s"].as_f64().unwrap_or(0.0).partial_cmp(&b["s"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
    items.truncate(MAX_ITEMS);
    items
}

// ---------- 该提醒的 ----------
fn ago(sec: f64) -> String {
    let m = py::round0(sec / 60.0).max(0);
    if m < 60 {
        return if m > 0 { format!("{m} 分钟后") } else { "马上".into() };
    }
    let (h, m) = (m / 60, m % 60);
    format!("{h} 小时{}后", if m > 0 { format!(" {m} 分钟") } else { String::new() })
}

fn wd(t: &NaiveDateTime) -> &'static str {
    WEEK[t.weekday().num_days_from_monday() as usize]
}

fn event(it: &Value, now: f64, tz: Zone) -> Value {
    let (s, e) = (it["s"].as_f64().unwrap_or(0.0), it["e"].as_f64().unwrap_or(0.0));
    let (a, b) = (local(s, tz), local(e, tz));
    let day = format!("{} 周{}", a.format("%Y-%m-%d"), wd(&a));
    let tt = py::str_of(&it["t"]);
    let (when_, title) = if it["d"] == true {
        let last = local(e - 1.0, tz);
        let tail = if last.date() != a.date() { format!("，到 {} 周{}", last.format("%m-%d"), wd(&last)) } else { String::new() };
        (format!("{day} 全天{tail}"), format!("今天全天：「{tt}」"))
    } else {
        let span = if b.date() == a.date() { format!("{}–{}", a.format("%H:%M"), b.format("%H:%M")) } else { format!("{} 到 {}", a.format("%H:%M"), b.format("%Y-%m-%d %H:%M")) };
        let w = if e > s { format!("{day} {span}") } else { format!("{day} {}", a.format("%H:%M")) };
        (w, format!("「{tt}」{}开始（{}）", ago(s - now), a.format("%H:%M")))
    };
    let g = |k: &str| it[k].as_str().unwrap_or("").to_string();
    let lines = [
        format!("时间：{when_}"),
        if g("l").is_empty() { String::new() } else { format!("地点：{}", g("l")) },
        if g("o").is_empty() { String::new() } else { format!("组织者：{}", g("o")) },
        if g("p").is_empty() { String::new() } else { format!("参加的人：{}", g("p")) },
        if g("x").is_empty() { String::new() } else { format!("说明：\n{}", g("x")) },
    ];
    json!({"id": it["k"], "title": title, "url": g("u"), "text": lines.iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join("\n")})
}

pub fn due(params: &Value, st: &mut Value, now: f64, default_local: Zone) -> Vec<Value> {
    let mb = if params["minutes_before"].is_null() { 15.0 } else { params["minutes_before"].as_f64().unwrap_or(15.0) } * 60.0;
    if py::truthy(&st["at"]) {
        let mut gaps: Vec<Value> = st["gaps"].as_array().cloned().unwrap_or_default();
        let n = gaps.len();
        gaps = gaps.split_off(n.saturating_sub(4));
        gaps.push(json!(py::round0(now - st["at"].as_f64().unwrap_or(0.0))));
        st["gaps"] = json!(gaps);
    }
    let mut gaps: Vec<f64> = st["gaps"].as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
    gaps.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let gap = if gaps.is_empty() { 300.0 } else { (gaps[1.min(gaps.len() - 1)] * 1.25).max(60.0).min(3600.0) };
    let tz = zone(st["tz"].as_str().unwrap_or(""), None).unwrap_or(default_local);
    let fired: HashSet<String> = st["fired"].as_array().map(|a| a.iter().map(|x| py::str_of(&x[0])).collect()).unwrap_or_default();
    let (mut out, mut new) = (vec![], vec![]);
    for it in st["items"].as_array().cloned().unwrap_or_default() {
        let k = py::str_of(&it["k"]);
        if fired.contains(&k) {
            continue;
        }
        let s = it["s"].as_f64().unwrap_or(0.0);
        let ok = if it["d"] == true {
            let at = s + ALL_DAY_AT * 3600.0;
            py::truthy(&params["all_day"]) && at <= now && now < s + 86400.0
        } else {
            now < s && (now >= s - mb || now + gap >= s)
        };
        if ok {
            out.push(event(&it, now, tz));
            new.push(json!([it["k"], it["s"]]));
        }
    }
    let mut f: Vec<Value> = st["fired"].as_array().map(|a| a.iter().filter(|x| x[1].as_f64().unwrap_or(0.0) >= now - 2.0 * 86400.0).cloned().collect()).unwrap_or_default();
    f.extend(new);
    let n = f.len();
    st["fired"] = json!(f.split_off(n.saturating_sub(MAX_FIRED)));
    let items: Vec<Value> = st["items"]
        .as_array()
        .map(|a| a.iter().filter(|i| i["e"].as_f64().unwrap_or(0.0) >= now || (i["d"] == true && i["s"].as_f64().unwrap_or(0.0) + 86400.0 > now)).cloned().collect())
        .unwrap_or_default();
    st["items"] = json!(items);
    st["at"] = json!(now);
    out
}

pub fn norm_url(u: &str) -> Result<String, String> {
    static WEBCAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^webcals?://").unwrap());
    static HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://[^\s/]+").unwrap());
    let mut u = u.trim().to_string();
    if WEBCAL.is_match(&u) {
        u = format!("https://{}", u.split_once("://").map(|x| x.1).unwrap_or(""));
    }
    if !HTTP.is_match(&u) {
        return Err("日历地址要以 https://、http:// 或 webcal:// 开头".into());
    }
    Ok(u)
}

fn sha1_12(s: &str) -> String {
    let d = sha1::Sha1::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..12].to_string()
}

pub async fn check_at(params: Value, state: Value, now: f64, default_local: Zone) -> Result<(Vec<Value>, Value), String> {
    let mut st = if state.is_object() { state } else { json!({}) };
    let url = norm_url(params["url"].as_str().unwrap_or(""))?;
    let src = sha1_12(&url);
    if st["src"] != src.as_str() || now - st["fetched"].as_f64().unwrap_or(0.0) >= FETCH_EVERY {
        let got: Result<(), String> = async {
            let r = util::get(&url, 30.0, 20_000_000, &[("Accept", "text/calendar, */*".into())], &[]).await.map_err(|e| e.text())?;
            let text_ = String::from_utf8_lossy(&r.body).into_owned();
            if !py::head(&text_, 5000).to_uppercase().contains("BEGIN:VCALENDAR") {
                return Err("这个地址返回的不是日历（ICS）：检查一下是不是订阅地址，或者日历有没有设成公开 / 能订阅".into());
            }
            let cal = parse(&text_);
            let items = occurrences(&cal, now, default_local);
            let o = st.as_object_mut().unwrap();
            for (k, v) in [("src", json!(src)), ("fetched", json!(now)), ("items", json!(items)), ("tz", json!(cal.tz))] {
                o.insert(k.into(), v);
            }
            Ok(())
        }
        .await;
        if let Err(e) = got {
            if st["src"] != src.as_str() || now - st["fetched"].as_f64().unwrap_or(0.0) > STALE {
                return Err(e);
            }
        }
    }
    let ev = due(&params, &mut st, now, default_local);
    Ok((ev, st))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("calendar");
    s.check = Some(|p, st| Box::pin(check_at(p, st, py::now(), Zone::Local)));
    s.validate = Some(|p| {
        let r = py::str_of(&p["url"]);
        let r = if p["url"].is_null() { String::new() } else { r };
        let raw = match r.strip_prefix("vault:") {
            Some(name) => vault::get(name).ok_or_else(|| format!("保险库里没有 {name}"))?,
            None => r,
        };
        norm_url(&raw)?;
        Ok(p)
    });
    s.summary = Some(|p| {
        let mb = if p["minutes_before"].is_null() { json!(15) } else { p["minutes_before"].clone() };
        let s = crate::t!("提前 {n} 分钟", n = mb);
        s + &if py::truthy(&p["all_day"]) { crate::t!("，含全天日程") } else { String::new() }
    });
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICS: &str = "BEGIN:VCALENDAR\r\nX-WR-TIMEZONE:Asia/Shanghai\r\nBEGIN:VEVENT\r\nUID:a1\r\nDTSTART;TZID=China Standard Time:20261120T100000\r\nDTEND;TZID=China Standard Time:20261120T110000\r\nSUMMARY:周会\\, 第一次\r\nLOCATION:3 楼\r\nORGANIZER;CN=Alice:mailto:alice@x.com\r\nATTENDEE;CN=Bob:mailto:bob@x.com\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:r1\r\nDTSTART:20261101T020000Z\r\nDURATION:PT30M\r\nRRULE:FREQ=DAILY;COUNT=40\r\nEXDATE:20261121T020000Z\r\nSUMMARY:站会\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:d1\r\nDTSTART;VALUE=DATE:20261120\r\nSUMMARY:生日\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn at(h: u32, m: u32) -> f64 {
        chrono_tz::Asia::Shanghai.with_ymd_and_hms(2026, 11, 20, h, m, 0).unwrap().timestamp() as f64
    }

    #[test]
    fn expand_and_due() {
        // 期望值是 Python 版同样的输入跑出来的（osa/sources/calendar.py，LOCAL=Asia/Shanghai）
        let cal = parse(ICS);
        let sh = Zone::Iana(chrono_tz::Asia::Shanghai);
        let items = occurrences(&cal, at(9, 0), sh);
        let want: Value = serde_json::from_str(r#"[{"k": "d1|1795104000", "s": 1795104000.0, "e": 1795190400.0, "d": true, "t": "生日", "l": "", "x": "", "u": "", "p": "", "o": ""}, {"k": "a1|1795140000", "s": 1795140000.0, "e": 1795143600.0, "d": false, "t": "周会, 第一次", "l": "3 楼", "x": "", "u": "", "p": "Bob <bob@x.com>", "o": "Alice <alice@x.com>"}, {"k": "r1|1795140000", "s": 1795140000.0, "e": 1795141800.0, "d": false, "t": "站会", "l": "", "x": "", "u": "", "p": "", "o": ""}]"#).unwrap();
        assert_eq!(json!(items), want);
        assert_eq!(py::dumps_u(&json!(items)), py::dumps_u(&want));
        let mut st = json!({"items": items, "tz": "Asia/Shanghai"});
        let ev = due(&json!({"minutes_before": 15, "all_day": true}), &mut st, at(9, 50), sh);
        let want: Value = serde_json::from_str(r#"[{"id": "d1|1795104000", "title": "今天全天：「生日」", "url": "", "text": "时间：2026-11-20 周五 全天"}, {"id": "a1|1795140000", "title": "「周会, 第一次」10 分钟后开始（10:00）", "url": "", "text": "时间：2026-11-20 周五 10:00–11:00\n地点：3 楼\n组织者：Alice <alice@x.com>\n参加的人：Bob <bob@x.com>"}, {"id": "r1|1795140000", "title": "「站会」10 分钟后开始（10:00）", "url": "", "text": "时间：2026-11-20 周五 10:00–10:30"}]"#).unwrap();
        assert_eq!(json!(ev), want);
        let again = due(&json!({"minutes_before": 15, "all_day": true}), &mut st, at(9, 55), sh);
        assert!(again.is_empty());
    }

    #[test]
    fn zones_and_misc() {
        assert_eq!(zone("China Standard Time", None), Some(Zone::Iana(chrono_tz::Asia::Shanghai)));
        assert_eq!(zone("/mozilla.org/20050126_1/Asia/Tokyo", None), Some(Zone::Iana(chrono_tz::Asia::Tokyo)));
        assert_eq!(zone("(UTC+08:00) Beijing", None), Some(Zone::Fixed(8 * 3600)));
        assert_eq!(duration("PT1H30M"), Some(5400.0));
        assert_eq!(ago(30.0 * 60.0), "30 分钟后");
        assert_eq!(ago(90.0 * 60.0), "1 小时 30 分钟后");
        assert_eq!(norm_url("webcal://x.com/a.ics").unwrap(), "https://x.com/a.ics");
        assert!(norm_url("ftp://x").is_err());
    }
}
