//! 迷你 JSON：扁平对象取值 + 字符串转义。
//! 只覆盖本项目需要的场景（前端/ curl 发来的 {"stuno","name","total"}），
//! 语义与旧版 C++ 的 jsonGet/jsonEscape 保持一致。

/// JSON 对象中取到的值：字符串或数字。
pub enum Value {
    Str(String),
    Num(f64),
}

/// C++ 版 isspace 的对应物（含 \v 与 \r）。
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

fn hex_val(c: u8) -> i32 {
    match c {
        b'0'..=b'9' => (c - b'0') as i32,
        b'a'..=b'f' => (c - b'a' + 10) as i32,
        b'A'..=b'F' => (c - b'A' + 10) as i32,
        _ => -1,
    }
}

/// 按码位追加 UTF-8 编码（含 \uXXXX 代理对合并）。
fn utf8_append(out: &mut Vec<u8>, cp: u32) {
    let cp = char::from_u32(cp).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(cp.encode_utf8(&mut buf).as_bytes());
}

/// 反转义 JSON 字符串内容（入参不含首尾引号）。
fn unescape(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' || i + 1 >= raw.len() {
            out.push(raw[i]);
            i += 1;
            continue;
        }
        let c = raw[i + 1];
        i += 2;
        match c {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                if i + 4 > raw.len() {
                    out.push(b'u');
                    continue;
                }
                let mut cp = 0u32;
                let mut ok = true;
                for k in 0..4 {
                    let h = hex_val(raw[i + k]);
                    if h < 0 {
                        ok = false;
                        break;
                    }
                    cp = cp * 16 + h as u32;
                }
                if !ok {
                    out.push(b'u');
                    continue;
                }
                i += 4;
                // 代理对：高代理 + \u 低代理 → 合成一个码位
                if (0xD800..=0xDBFF).contains(&cp)
                    && i + 6 < raw.len()
                    && raw[i] == b'\\'
                    && raw[i + 1] == b'u'
                {
                    let mut lo = 0u32;
                    let mut ok2 = true;
                    for k in 2..6 {
                        let h = hex_val(raw[i + k]);
                        if h < 0 {
                            ok2 = false;
                            break;
                        }
                        lo = lo * 16 + h as u32;
                    }
                    if ok2 && (0xDC00..=0xDFFF).contains(&lo) {
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        i += 6;
                    }
                }
                utf8_append(&mut out, cp);
            }
            _ => out.push(c),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 在扁平 JSON 对象中按键取值；字符串返回 Value::Str，数字返回 Value::Num。
/// 找不到键、结构不符（如键名前不是 `{`/`,`/空格）时返回 None。
pub fn get(body: &str, key: &str) -> Option<Value> {
    let b = body.as_bytes();
    let pat = format!("\"{}\"", key);
    let mut p = find(b, pat.as_bytes())?;
    if p > 0 && b[p - 1] != b'{' && b[p - 1] != b',' && b[p - 1] != b' ' {
        return None;
    }
    p += pat.len();
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if p >= b.len() || b[p] != b':' {
        return None;
    }
    p += 1;
    while p < b.len() && is_space(b[p]) {
        p += 1;
    }
    if p >= b.len() {
        return None;
    }

    if b[p] == b'"' {
        // 扫到配对引号，保留转义序列原样交给 unescape
        let mut raw = Vec::new();
        let mut e = p + 1;
        while e < b.len() {
            if b[e] == b'\\' && e + 1 < b.len() {
                raw.push(b[e]);
                raw.push(b[e + 1]);
                e += 2;
                continue;
            }
            if b[e] == b'"' {
                break;
            }
            raw.push(b[e]);
            e += 1;
        }
        if e >= b.len() {
            return None;
        }
        return Some(Value::Str(unescape(&raw)));
    }

    let mut e = p;
    while e < b.len() && b[e] != b',' && b[e] != b'}' && b[e] != b' ' {
        e += 1;
    }
    let tok = std::str::from_utf8(&b[p..e]).ok()?.trim();
    if tok.is_empty() {
        return None;
    }
    // 非数字令牌一律视为非法（旧版 strtod 会把 "abc" 当成 0，属于漏洞）
    let n: f64 = tok.parse().ok()?;
    Some(Value::Num(n))
}

/// 找子串（字节级），返回起始下标。
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 转义 JSON 字符串内容（返回值不含首尾引号）。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
