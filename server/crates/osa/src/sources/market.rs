//! 价格到线：A 股、港股、美股（腾讯行情）、加密货币（CoinGecko）、外汇（open.er-api.com）。对应 osa/sources/market.py。

use super::util;
use crate::py;
use crate::registry::Source;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

const MARGIN: f64 = 0.005;

const CRYPTO: [(&str, &str); 33] = [
    ("BTC", "bitcoin"), ("ETH", "ethereum"), ("USDT", "tether"), ("USDC", "usd-coin"), ("BNB", "binancecoin"), ("SOL", "solana"),
    ("XRP", "ripple"), ("DOGE", "dogecoin"), ("ADA", "cardano"), ("TRX", "tron"), ("TON", "the-open-network"), ("AVAX", "avalanche-2"),
    ("DOT", "polkadot"), ("LINK", "chainlink"), ("LTC", "litecoin"), ("BCH", "bitcoin-cash"), ("SHIB", "shiba-inu"),
    ("POL", "polygon-ecosystem-token"), ("MATIC", "matic-network"), ("UNI", "uniswap"), ("ATOM", "cosmos"), ("XLM", "stellar"),
    ("ETC", "ethereum-classic"), ("FIL", "filecoin"), ("APT", "aptos"), ("ARB", "arbitrum"), ("OP", "optimism"), ("SUI", "sui"),
    ("PEPE", "pepe"), ("NEAR", "near"), ("ICP", "internet-computer"), ("HBAR", "hedera-hashgraph"), ("XMR", "monero"),
];

fn crypto_id(base: &str) -> Option<&'static str> {
    CRYPTO.iter().find(|(k, _)| *k == base).map(|(_, v)| *v)
}

fn ccy(market: &str) -> &'static str {
    match market {
        "cn" => "CNY",
        "hk" => "HKD",
        _ => "USD",
    }
}

fn exchange(code: &str) -> &'static str {
    if code.starts_with("92") || code.starts_with('4') || code.starts_with('8') {
        "bj"
    } else if code.starts_with('6') || code.starts_with('9') || code.starts_with('5') {
        "sh"
    } else {
        "sz"
    }
}

/// 把用户写的代码整理成统一的样子
pub fn norm_symbol(market: &str, symbol: &str) -> Result<String, String> {
    static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
    static CN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(sh|sz|bj)?(\d{6})(?:\.(sh|sz|bj|ss))?$").unwrap());
    static IDX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z]{2,10}$").unwrap());
    static US: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9.\-]{0,9}$").unwrap());
    static BASE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9\-]{1,60}$").unwrap());
    static QUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Z]{3,5}$").unwrap());
    static FX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z]{3})[/\-_]?([A-Za-z]{3})$").unwrap());
    let s = WS.replace_all(symbol, "").into_owned();
    if s.is_empty() {
        return Err("要填代码".into());
    }
    match market {
        "cn" => {
            let low = s.to_lowercase();
            let m = CN.captures(&low).ok_or("A 股代码是 6 位数字，比如 600519、sz000001（上证指数写 sh000001）")?;
            let ex = match (m.get(1), m.get(3)) {
                (Some(a), _) => a.as_str().to_string(),
                (None, Some(b)) if b.as_str() == "ss" => "sh".into(),
                (None, Some(b)) => b.as_str().to_string(),
                (None, None) => exchange(&m[2]).into(),
            };
            Ok(format!("{ex}{}", &m[2]))
        }
        "hk" => {
            let up = s.to_uppercase();
            let up = up.strip_prefix("HK").unwrap_or(&up);
            let up = up.strip_suffix(".HK").unwrap_or(up);
            if !up.is_empty() && up.chars().all(|c| c.is_ascii_digit()) && up.len() <= 5 {
                return Ok(format!("{up:0>5}"));
            }
            if IDX.is_match(up) {
                return Ok(up.to_string());
            }
            Err("港股代码是 5 位数字，比如 00700".into())
        }
        "us" => {
            let up = s.to_uppercase();
            if !US.is_match(&up) {
                return Err("美股写代号，比如 AAPL、TSLA".into());
            }
            Ok(up)
        }
        "crypto" => {
            let (base, quote) = s.split_once('/').unwrap_or((&s, ""));
            let quote = if quote.is_empty() { "USD".to_string() } else { quote.to_uppercase() };
            if !BASE.is_match(base) || !QUOTE.is_match(&quote) {
                return Err("加密货币写代号（BTC）或 CoinGecko 的 id（bitcoin），要人民币价格写 BTC/CNY".into());
            }
            let base = if crypto_id(&base.to_uppercase()).is_some() { base.to_uppercase() } else { base.to_lowercase() };
            Ok(if quote == "USD" { base } else { format!("{base}/{quote}") })
        }
        "fx" => {
            let m = FX.captures(&s).ok_or("外汇写货币对，比如 USD/CNY")?;
            Ok(format!("{}/{}", &m[1], &m[2]).to_uppercase())
        }
        _ => Err(format!("不认识的市场：{market}")),
    }
}

/// 价格的写法：大数两位小数加千位分隔，1 到 100 最多 4 位小数，小于 1 的 6 位有效数字
pub fn num(x: f64) -> String {
    if x.abs() >= 100.0 {
        util::comma(x, 2)
    } else if x.abs() >= 1.0 {
        let s = util::comma(x, 4);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        util::g(x, 6)
    }
}

fn clock(ts: f64) -> String {
    if ts == 0.0 { String::new() } else { py::strftime_at(ts, "%Y-%m-%d %H:%M") }
}

fn gm_day(ts: f64) -> String {
    chrono::DateTime::from_timestamp(ts.floor() as i64, 0).unwrap_or_default().format("%Y-%m-%d").to_string()
}

fn tencent_time(s: &str) -> String {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if d.len() >= 12 { format!("{}-{}-{} {}:{}", &d[..4], &d[4..6], &d[6..8], &d[8..10], &d[10..12]) } else { s.to_string() }
}

fn ferr(e: util::FetchErr) -> String {
    e.text()
}

async fn stock(market: &str, symbol: &str) -> Result<Value, String> {
    static V: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"v_[\w.\-]*="([^"]*)""#).unwrap());
    let q = if market == "cn" { symbol.to_string() } else { format!("{market}{symbol}") };
    let r = util::get(&format!("https://qt.gtimg.cn/q={q}"), 15.0, 200_000, &[], &[]).await.map_err(ferr)?;
    let text = util::decode(&r.body, "gbk");
    let f: Vec<&str> = V.captures(&text).map(|m| m.get(1).map(|x| x.as_str()).unwrap_or("").split('~').collect()).unwrap_or_default();
    if f.len() < 33 || f[1].is_empty() || text.contains("none_match") {
        return Err(format!("没找到 {symbol} 的行情，检查一下代码和市场选对了没有"));
    }
    let pf = |s: &str| -> Result<f64, String> {
        if s.is_empty() { Ok(0.0) } else { s.parse::<f64>().map_err(|_| format!("could not convert string to float: '{s}'")) }
    };
    let (price, prev) = (pf(f[3])?, pf(f[4])?);
    let pct = match f[32].parse::<f64>() {
        Ok(p) => json!(p),
        Err(_) => {
            if prev != 0.0 {
                json!((price / prev - 1.0) * 100.0)
            } else {
                Value::Null
            }
        }
    };
    let when = tencent_time(f[30]);
    Ok(json!({"name": f[1], "code": symbol, "price": price, "prev": prev, "pct": pct, "time": when,
              "day": py::head(&when, 10), "ccy": ccy(market)}))
}

async fn crypto(symbol: &str) -> Result<Value, String> {
    let (base, quote) = symbol.split_once('/').unwrap_or((symbol, ""));
    let quote = if quote.is_empty() { "usd".to_string() } else { quote.to_lowercase() };
    let cid = crypto_id(&base.to_uppercase()).map(String::from).unwrap_or_else(|| base.to_lowercase());
    let r = util::get(
        "https://api.coingecko.com/api/v3/simple/price",
        15.0,
        200_000,
        &[],
        &[
            ("ids", cid.clone()),
            ("vs_currencies", quote.clone()),
            ("include_24hr_change", "true".into()),
            ("include_last_updated_at", "true".into()),
        ],
    )
    .await
    .map_err(ferr)?;
    let d = r.json()[&cid].clone();
    if d[&quote].is_null() {
        return Err(format!("CoinGecko 上找不到 {symbol}：写常见代号（BTC、ETH）或者币的 id（bitcoin）"));
    }
    let ts = if py::truthy(&d["last_updated_at"]) { d["last_updated_at"].as_f64().unwrap_or(py::now()) } else { py::now() };
    Ok(json!({"name": symbol, "code": symbol, "price": d[&quote].as_f64().unwrap_or(0.0), "prev": Value::Null,
              "pct": d[format!("{quote}_24h_change")], "time": clock(ts), "day": gm_day(ts), "ccy": quote.to_uppercase(), "pct_what": "24 小时"}))
}

async fn fx(symbol: &str, st: &mut Value) -> Result<Value, String> {
    let (a, b) = symbol.split_once('/').unwrap_or((symbol, ""));
    let r = util::get(&format!("https://open.er-api.com/v6/latest/{a}"), 15.0, 500_000, &[], &[]).await.map_err(ferr)?;
    let d = r.json();
    if d["result"] != "success" {
        let why = if py::truthy(&d["error-type"]) { py::str_of(&d["error-type"]) } else { "接口没返回数据".into() };
        return Err(format!("查不到 {a} 的汇率：{why}"));
    }
    let rate = d["rates"][b].clone();
    let Some(rate) = rate.as_f64() else { return Err(format!("没有 {b} 这种货币的汇率")) };
    let ts = if py::truthy(&d["time_last_update_unix"]) { d["time_last_update_unix"].clone() } else { json!(0) };
    let old = if st["fx"]["code"] == symbol { st["fx"].clone() } else { json!({}) };
    let mut prev = old["prev"].clone();
    if py::truthy(&old["t"]) && old["t"] != ts {
        prev = old["rate"].clone();
    }
    st["fx"] = json!({"code": symbol, "t": ts, "rate": d["rates"][b], "prev": prev});
    let tsf = ts.as_f64().unwrap_or(0.0);
    let pct = match prev.as_f64() {
        Some(p) if p != 0.0 => json!((rate / p - 1.0) * 100.0),
        _ => Value::Null,
    };
    Ok(json!({"name": symbol, "code": symbol, "price": rate, "prev": prev, "pct": pct, "time": clock(tsf),
              "day": gm_day(tsf), "ccy": b, "pct_what": "和上一次更新比"}))
}

pub async fn quote(params: &Value, st: &mut Value) -> Result<Value, String> {
    let market = if py::truthy(&params["market"]) { py::str_of(&params["market"]) } else { "cn".into() };
    let symbol = norm_symbol(&market, params["symbol"].as_str().unwrap_or(""))?;
    match market.as_str() {
        "cn" | "hk" | "us" => stock(&market, &symbol).await,
        "crypto" => crypto(&symbol).await,
        _ => fx(&symbol, st).await,
    }
}

fn cond_text(cond: &str, th: &Value) -> String {
    let th = py::str_of(th);
    match cond {
        "above" => format!("涨到 {th} 以上"),
        "below" => format!("跌到 {th} 以下"),
        _ => format!("涨跌幅超过 {th}%"),
    }
}

/// 拿到行情以后：该不该报、新的 state
pub fn judge(params: &Value, q: &Value, state: &Value) -> (Vec<Value>, Value) {
    let mut st = if state.is_object() { state.clone() } else { json!({}) };
    let cond = if py::truthy(&params["condition"]) { py::str_of(&params["condition"]) } else { "above".into() };
    let th = if py::truthy(&params["threshold"]) { params["threshold"].as_f64().unwrap_or(0.0) } else { 0.0 };
    let price = q["price"].as_f64().unwrap_or(0.0);
    let pct = q["pct"].as_f64();
    if price == 0.0 {
        return (vec![], st);
    }
    let mut fire = false;
    if cond == "above" || cond == "below" {
        let hit = if cond == "above" { price >= th } else { price <= th };
        let back = if cond == "above" { price < th * (1.0 - MARGIN) } else { price > th * (1.0 + MARGIN) };
        if hit && !py::truthy(&st["hit"]) {
            fire = true;
            st["hit"] = json!(true);
        } else if py::truthy(&st["hit"]) && back {
            st["hit"] = json!(false);
        }
    } else if let Some(p) = pct {
        if p.abs() >= th && st["day"] != q["day"] {
            fire = true;
            st["day"] = q["day"].clone();
        }
    }
    st["last"] = q["price"].clone();
    if !fire {
        return (vec![], st);
    }
    let sign = match pct {
        Some(p) => format!("{}{}%", if p >= 0.0 { "+" } else { "" }, util::f(p, 2)),
        None => "—".into(),
    };
    let what = cond_text(&cond, &params["threshold"]);
    let code = if q["name"] == q["code"] { format!("{} ", py::str_of(&q["code"])) } else { format!("{}（{}）", py::str_of(&q["name"]), py::str_of(&q["code"])) };
    let pct_what = if q["pct_what"].is_string() { py::str_of(&q["pct_what"]) } else { "今天".into() };
    let prev = match q["prev"].as_f64() {
        Some(p) if p != 0.0 && q["pct_what"].is_null() => format!("，昨收 {}", num(p)),
        _ => String::new(),
    };
    let time = if py::truthy(&q["time"]) { format!("行情时间：{}", py::str_of(&q["time"])) } else { String::new() };
    let lines = [format!("现价：{} {}", num(price), py::str_of(&q["ccy"])), format!("涨跌：{sign}（{pct_what}{prev}）"), time, format!("条件：{what}")];
    let tid = if py::truthy(&q["time"]) { py::str_of(&q["time"]) } else { py::str_of(&json!(py::now())) };
    (
        vec![json!({"id": format!("{}-{cond}-{tid}", py::str_of(&q["code"])), "title": format!("{code}{what}：现价 {}（{sign}）", num(price)),
                    "text": lines.iter().filter(|x| !x.is_empty()).cloned().collect::<Vec<_>>().join("\n")})],
        st,
    )
}

pub async fn check(params: Value, state: Value) -> Result<(Vec<Value>, Value), String> {
    let mut st = if state.is_object() { state } else { json!({}) };
    let q = quote(&params, &mut st).await?;
    Ok(judge(&params, &q, &st))
}

pub fn sources() -> Vec<Source> {
    let mut s = super::base("price");
    s.check = Some(|p, st| Box::pin(check(p, st)));
    s.validate = Some(|mut p| {
        if !py::truthy(&p["market"]) {
            p["market"] = json!("cn");
        }
        if !py::truthy(&p["condition"]) {
            p["condition"] = json!("above");
        }
        let m = py::str_of(&p["market"]);
        p["symbol"] = json!(norm_symbol(&m, p["symbol"].as_str().unwrap_or(""))?);
        match p["threshold"].as_f64() {
            Some(t) if t > 0.0 => Ok(p),
            _ => Err("要填一个大于 0 的数：到价写价格，涨跌幅写百分比".into()),
        }
    });
    s.summary = Some(|p| {
        let th = py::str_of(&p["threshold"]);
        let cond = if py::truthy(&p["condition"]) { py::str_of(&p["condition"]) } else { "above".into() };
        let sym = if p.get("symbol").is_some() { py::str_of(&p["symbol"]) } else { String::new() };
        format!(
            "{sym} {}",
            match cond.as_str() {
                "above" => format!("≥ {th}"),
                "below" => format!("≤ {th}"),
                _ => format!("±{th}%"),
            }
        )
    });
    vec![s]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols() {
        assert_eq!(norm_symbol("cn", "600519").unwrap(), "sh600519");
        assert_eq!(norm_symbol("cn", "000001").unwrap(), "sz000001");
        assert_eq!(norm_symbol("cn", "600519.SS").unwrap(), "sh600519");
        assert_eq!(norm_symbol("hk", "700").unwrap(), "00700");
        assert_eq!(norm_symbol("hk", "hsi").unwrap(), "HSI");
        assert_eq!(norm_symbol("crypto", "btc").unwrap(), "BTC");
        assert_eq!(norm_symbol("crypto", "btc/cny").unwrap(), "BTC/CNY");
        assert_eq!(norm_symbol("crypto", "Bitcoin").unwrap(), "bitcoin");
        assert_eq!(norm_symbol("fx", "usdcny").unwrap(), "USD/CNY");
        assert!(norm_symbol("cn", "12345").is_err());
        assert_eq!(num(1688.0), "1,688.00");
        assert_eq!(num(7.1234), "7.1234");
        assert_eq!(num(7.1), "7.1");
        assert_eq!(num(0.000123456), "0.000123456");
    }

    #[tokio::test]
    async fn stock_cross_once() {
        let fields: Vec<String> = (0..50).map(|i| match i {
            1 => "贵州茅台".into(),
            3 => "1700.50".into(),
            4 => "1650.00".into(),
            30 => "20261119150003".into(),
            32 => "3.06".into(),
            _ => "x".into(),
        }).collect();
        let body = format!("v_sh600519=\"{}\";", fields.join("~"));
        let (gbk, _, _) = encoding_rs::GB18030.encode(&body);
        let gbk = gbk.into_owned();
        let mock: util::Mock = std::sync::Arc::new(move |_u: &reqwest::Url| (200, gbk.clone()));
        let p = json!({"market": "cn", "symbol": "600519", "condition": "above", "threshold": 1680});
        let (ev, st) = util::MOCK.scope(mock.clone(), async { check(p.clone(), Value::Null).await.unwrap() }).await;
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["title"], "贵州茅台（sh600519）涨到 1680 以上：现价 1,700.50（+3.06%）");
        assert_eq!(ev[0]["text"], "现价：1,700.50 CNY\n涨跌：+3.06%（今天，昨收 1,650.00）\n行情时间：2026-11-19 15:00\n条件：涨到 1680 以上");
        assert_eq!(ev[0]["id"], "sh600519-above-2026-11-19 15:00");
        let (again, _) = util::MOCK.scope(mock, async { check(p, st).await.unwrap() }).await;
        assert!(again.is_empty());
    }
}
