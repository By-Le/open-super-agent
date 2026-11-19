//! 小工具：fnmatch、posixpath.normpath、base64、zlib、随机串。

use base64::Engine;

/// fnmatch.fnmatchcase：* ? [seq] [!seq]
pub fn fnmatch(name: &str, pat: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    fn class(p: &[char], i: usize, c: char) -> Option<(bool, usize)> {
        // 返回 (命中没有, 类结束后的位置)；不是合法的类返回 None（按字面的 [ 处理）
        let mut j = i + 1;
        let neg = j < p.len() && p[j] == '!';
        if neg {
            j += 1;
        }
        let start = j;
        let mut hit = false;
        while j < p.len() && (p[j] != ']' || j == start) {
            if j + 2 < p.len() && p[j + 1] == '-' && p[j + 2] != ']' {
                if p[j] <= c && c <= p[j + 2] {
                    hit = true;
                }
                j += 3;
            } else {
                if p[j] == c {
                    hit = true;
                }
                j += 1;
            }
        }
        if j >= p.len() {
            return None;
        }
        Some((hit != neg, j + 1))
    }
    fn go(n: &[char], p: &[char], ni: usize, pi: usize) -> bool {
        let (mut ni, mut pi) = (ni, pi);
        let mut star: Option<(usize, usize)> = None;
        loop {
            if pi < p.len() {
                match p[pi] {
                    '*' => {
                        star = Some((pi, ni));
                        pi += 1;
                        continue;
                    }
                    '?' if ni < n.len() => {
                        ni += 1;
                        pi += 1;
                        continue;
                    }
                    '[' if ni < n.len() => match class(p, pi, n[ni]) {
                        Some((true, next)) => {
                            ni += 1;
                            pi = next;
                            continue;
                        }
                        Some((false, _)) => {}
                        None => {
                            if n[ni] == '[' {
                                ni += 1;
                                pi += 1;
                                continue;
                            }
                        }
                    },
                    c if ni < n.len() && c == n[ni] && c != '?' && c != '[' => {
                        ni += 1;
                        pi += 1;
                        continue;
                    }
                    _ => {}
                }
            } else if ni == n.len() {
                return true;
            }
            match star {
                Some((sp, sn)) if sn < n.len() => {
                    star = Some((sp, sn + 1));
                    pi = sp + 1;
                    ni = sn + 1;
                }
                _ => return false,
            }
        }
    }
    go(&n, &p, 0, 0)
}

/// posixpath.normpath
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = vec![];
    for comp in path.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(comp);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = parts.join("/");
    let out = format!("{}{}", "/".repeat(initial), joined);
    if out.is_empty() { ".".into() } else { out }
}

pub fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).ok()
}

pub fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

pub fn zlib_compress(b: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = e.write_all(b);
    e.finish().unwrap_or_default()
}

/// secrets.token_urlsafe(n)
pub fn token_urlsafe(n: usize) -> String {
    let b: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    b64url(&b)
}

/// secrets.token_hex(n)
pub fn token_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

pub fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globs() {
        assert!(fnmatch("filesystem__write", "filesystem__*"));
        assert!(fnmatch("git status", "git *"));
        assert!(!fnmatch("gitx", "git *"));
        assert!(fnmatch("a", "[abc]"));
        assert!(!fnmatch("d", "[abc]"));
        assert!(fnmatch("d", "[!abc]"));
        assert_eq!(normpath("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normpath("/../x"), "/x");
    }
}
