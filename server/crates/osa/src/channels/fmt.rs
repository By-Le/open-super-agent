//! 把 Bot 的 Markdown 回复改成聊天软件能显示的样子。对应 osa/channels/fmt.py（每个函数的规则见那里）。

use crate::py;
use fancy_regex::{Captures, Regex};
use std::sync::LazyLock;

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

static FENCE: LazyLock<Regex> = LazyLock::new(|| re(r"^\s*(```|~~~)"));
static CODE_BLOCK: LazyLock<Regex> = LazyLock::new(|| re(r"(?sm)^([ \t]*)(```|~~~)([^\n]*)\n(.*?)(?:\n[ \t]*\2[ \t]*$|\z)"));
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| re(r"`([^`\n]+)`"));
static LINK: LazyLock<Regex> = LazyLock::new(|| re(r"\[([^\]\n]+)\]\(([^)\s]+)\)"));
static BOLD: LazyLock<Regex> = LazyLock::new(|| re(r"\*\*(.+?)\*\*|__(.+?)__"));
static ITALIC: LazyLock<Regex> = LazyLock::new(|| re(r"(?<![\w*])\*(?!\s)([^*\n]+?)(?<!\s)\*(?![\w*])"));
static STRIKE: LazyLock<Regex> = LazyLock::new(|| re(r"~~(.+?)~~"));
static HEADING: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^#{1,6}\s+(.+?)\s*#*\s*$"));

fn is_fence(line: &str) -> bool {
    FENCE.is_match(line).unwrap_or(false)
}

/// 正则替换（fancy_regex 版的 re.sub）
pub fn sub(r: &Regex, text: &str, f: impl Fn(&Captures) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for c in r.captures_iter(text).flatten() {
        let m = c.get(0).unwrap();
        out.push_str(&text[last..m.start()]);
        out.push_str(&f(&c));
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

fn g<'a>(c: &Captures<'a>, i: usize) -> &'a str {
    c.get(i).map(|m| m.as_str()).unwrap_or("")
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

/// 切成每段不超过 limit 个字符
pub fn split(text: &str, limit: usize) -> Vec<String> {
    let text = text.trim();
    if clen(text) <= limit {
        return if text.is_empty() { vec![] } else { vec![text.to_string()] };
    }
    let room = limit.saturating_sub(8).max(1);
    let mut out: Vec<String> = vec![];
    let mut cur: Vec<String> = vec![];
    let mut fence = false;
    let size = |lines: &[String]| -> usize { lines.iter().map(|x| clen(x) + 1).sum() };
    let flush = |cur: &mut Vec<String>, out: &mut Vec<String>, fence: bool| {
        let mut carry: Vec<String> = vec![];
        if !fence {
            let half = cur.len() / 2;
            let blanks: Vec<usize> = cur.iter().enumerate().filter(|(i, x)| x.trim().is_empty() && *i >= half).map(|(i, _)| i).collect();
            if let Some(&b) = blanks.last() {
                if !cur[b..].iter().any(|x| is_fence(x)) {
                    carry = cur[b + 1..].to_vec();
                    cur.truncate(b);
                }
            }
        }
        let body = cur.join("\n").trim_matches('\n').to_string();
        if !body.trim().is_empty() {
            out.push(body + if fence { "\n```" } else { "" });
        }
        *cur = if fence { vec!["```".to_string()] } else { vec![] };
        cur.extend(carry);
    };
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        let n = chars.len().max(1);
        let mut i = 0;
        while i < n {
            let piece: String = chars[i.min(chars.len())..(i + room).min(chars.len())].iter().collect();
            if !cur.is_empty() && size(&cur) + clen(&piece) + 1 > room {
                flush(&mut cur, &mut out, fence);
            }
            cur.push(piece);
            i += room;
        }
        if is_fence(line) {
            fence = !fence;
        }
    }
    let body = cur.join("\n").trim_matches('\n').to_string();
    if !body.trim().is_empty() {
        out.push(body);
    }
    out
}

/// html.escape
pub fn escape(s: &str, quote: bool) -> String {
    let mut o = s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    if quote {
        o = o.replace('"', "&quot;").replace('\'', "&#x27;");
    }
    o
}

static BULLET: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^(\s*)[-*+]\s+"));
static RULE: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^\s*([-*_])(\s*\1){2,}\s*$"));
static QUOTE: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)(?:^&gt; ?.*(?:\n|$))+"));
static QUOTE_MARK: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^&gt; ?"));
static TABLE: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)(?:^\|.*\|[ \t]*(?:\n|$))+"));
static HOLD: LazyLock<Regex> = LazyLock::new(|| re("\x00(\\d+)\x00"));

fn inline(text: &str) -> String {
    let keep = std::cell::RefCell::new(Vec::<String>::new());
    let hold = |s: String| -> String {
        let mut k = keep.borrow_mut();
        k.push(s);
        format!("\x00{}\x00", k.len() - 1)
    };
    let t = sub(&INLINE_CODE, text, |m| hold(format!("<code>{}</code>", escape(g(m, 1), true))));
    let t = sub(&LINK, &t, |m| hold(format!("<a href=\"{}\">{}</a>", escape(g(m, 2), true), escape(g(m, 1), true))));
    let t = escape(&t, false);
    let t = sub(&HEADING, &t, |m| format!("<b>{}</b>", g(m, 1)));
    let t = sub(&BOLD, &t, |m| format!("<b>{}</b>", if m.get(1).is_some() { g(m, 1) } else { g(m, 2) }));
    let t = sub(&ITALIC, &t, |m| format!("<i>{}</i>", g(m, 1)));
    let t = sub(&STRIKE, &t, |m| format!("<s>{}</s>", g(m, 1)));
    let t = sub(&BULLET, &t, |m| format!("{}• ", g(m, 1)));
    let t = sub(&RULE, &t, |_| "──────".to_string());
    let t = sub(&QUOTE, &t, |m| format!("<blockquote>{}</blockquote>\n", sub(&QUOTE_MARK, g(m, 0).trim_end_matches('\n'), |_| String::new())));
    let t = sub(&TABLE, &t, |m| format!("<pre>{}</pre>\n", g(m, 0).trim_end()));
    let k = keep.borrow();
    sub(&HOLD, &t, |m| k.get(g(m, 1).parse::<usize>().unwrap_or(0)).cloned().unwrap_or_default())
}

pub fn telegram_html(md: &str) -> String {
    let mut out = String::new();
    let mut pos = 0;
    for m in CODE_BLOCK.captures_iter(md).flatten() {
        let whole = m.get(0).unwrap();
        out.push_str(&inline(&md[pos..whole.start()]));
        let lang = g(&m, 3).trim().split(' ').next().unwrap_or("").to_string();
        let attr = if lang.is_empty() { String::new() } else { format!(" class=\"language-{}\"", escape(&lang, true)) };
        let indent = g(&m, 1);
        let code = if indent.is_empty() {
            g(&m, 4).to_string()
        } else {
            let r = re(&format!("(?m)^{}", fancy_regex::escape(indent)));
            sub(&r, g(&m, 4), |_| String::new())
        };
        out.push_str(&format!("<pre><code{attr}>{}</code></pre>", escape(&code, true)));
        pos = whole.end();
    }
    out.push_str(&inline(&md[pos..]));
    out.trim().to_string()
}

/// 退回纯文本时用：去掉最显眼的 Markdown 记号
pub fn plain(md: &str) -> String {
    let t = sub(&LINK, md, |m| crate::t!("{text}（{url}）", text = g(m, 1), url = g(m, 2)));
    let t = sub(&BOLD, &t, |m| if m.get(1).is_some() { g(m, 1).to_string() } else { g(m, 2).to_string() });
    sub(&HEADING, &t, |m| g(m, 1).to_string())
}

fn outside_code(md: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut pos = 0;
    for m in CODE_BLOCK.find_iter(md).flatten() {
        out.push_str(&f(&md[pos..m.start()]));
        out.push_str(m.as_str());
        pos = m.end();
    }
    out.push_str(&f(&md[pos..]));
    out
}

pub fn feishu_md(md: &str) -> String {
    outside_code(md, |t| sub(&HEADING, t, |h| format!("**{}**", g(h, 1))))
}

/// 按代码块切开：每个代码块单独一段
pub fn fence_segments(md: &str) -> Vec<String> {
    let mut out = vec![];
    let mut buf: Vec<&str> = vec![];
    let mut inside = false;
    for line in md.split('\n') {
        let f = is_fence(line);
        if f && !inside {
            if !buf.is_empty() {
                out.push(buf.join("\n"));
            }
            buf = vec![line];
            inside = true;
        } else if f && inside {
            buf.push(line);
            out.push(buf.join("\n"));
            buf = vec![];
            inside = false;
        } else {
            buf.push(line);
        }
    }
    if !buf.is_empty() {
        out.push(buf.join("\n"));
    }
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

static DING_BR: LazyLock<Regex> = LazyLock::new(|| re(r"(?<=\S)[ \t]*\n(?=[ \t]*\S)"));

pub fn dingtalk_md(md: &str) -> String {
    outside_code(md, |t| sub(&DING_BR, t, |_| "  \n".to_string()))
}

static DISCORD_H: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^#{4,6}\s+(.+?)\s*#*\s*$"));

pub fn discord_md(md: &str) -> String {
    outside_code(md, |t| {
        let t = sub(&DISCORD_H, t, |m| format!("**{}**", g(m, 1)));
        sub(&TABLE, &t, |m| format!("```\n{}\n```\n", g(m, 0).trim_end()))
    })
}

static TITLE_STRIP: LazyLock<Regex> = LazyLock::new(|| re(r"^[#>\-*+\s]+|[`*_~]"));

/// 一段 Markdown 的第一行纯文字，当通知标题用
pub fn title_line(md: &str, limit: usize) -> String {
    for line in plain(md).lines() {
        let l = sub(&TITLE_STRIP, line, |_| String::new());
        let l = l.trim();
        if !l.is_empty() {
            return py::head(l, limit);
        }
    }
    crate::t!("消息")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html() {
        assert_eq!(telegram_html("# 标题\n**粗** *斜* `a<b` [x](http://a)"), "<b>标题</b>\n<b>粗</b> <i>斜</i> <code>a&lt;b</code> <a href=\"http://a\">x</a>");
        assert_eq!(telegram_html("```py\nx<1\n```"), "<pre><code class=\"language-py\">x&lt;1</code></pre>");
        assert_eq!(dingtalk_md("a\nb"), "a  \nb");
        let parts = split(&"字".repeat(50), 20);
        assert!(parts.iter().all(|p| p.chars().count() <= 20));
    }
}
