//! Minimal JSON: a value tree, a parser and a writer. Standard library only.
//!
//! Hook payloads can be megabytes (a Write's `content`), so the parser is a
//! single pass over bytes. Objects keep insertion order. Numbers are f64,
//! which is exact for every integer agent-lock stores (times, pids, counts).

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

pub const NULL: Json = Json::Null;

impl Json {
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    /// Member of an object (last one wins, like JS). `NULL` when absent or not an object.
    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(m) => m.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(&NULL),
            _ => &NULL,
        }
    }

    /// Follow a path of object keys.
    pub fn at(&self, path: &[&str]) -> &Json {
        path.iter().fold(self, |v, k| v.get(k))
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Non-empty string.
    pub fn text(&self) -> Option<&str> {
        self.str().filter(|s| !s.is_empty())
    }

    pub fn u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 1.8e19 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }

    pub fn members(&self) -> &[(String, Json)] {
        match self {
            Json::Obj(m) => m,
            _ => &[],
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Set (or replace) a member of an object; no-op on non-objects.
    pub fn set(&mut self, key: &str, v: Json) {
        if let Json::Obj(m) = self {
            if let Some(slot) = m.iter_mut().find(|(k, _)| k == key) {
                slot.1 = v;
            } else {
                m.push((key.to_string(), v));
            }
        }
    }

    pub fn to_string(&self) -> String {
        let mut out = String::with_capacity(256);
        write_value(&mut out, self, None, 0);
        out
    }

    /// Two-space indented, for files people may `cat`.
    pub fn to_pretty(&self) -> String {
        let mut out = String::with_capacity(1024);
        write_value(&mut out, self, Some(2), 0);
        out.push('\n');
        out
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}
impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}
impl From<u64> for Json {
    fn from(n: u64) -> Json {
        Json::Num(n as f64)
    }
}
impl From<u32> for Json {
    fn from(n: u32) -> Json {
        Json::Num(n as f64)
    }
}
impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}
impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(o: Option<T>) -> Json {
        o.map(Into::into).unwrap_or(Json::Null)
    }
}

/// Build an object from (key, value) pairs, skipping nulls.
pub fn object<I: IntoIterator<Item = (&'static str, Json)>>(pairs: I) -> Json {
    Json::Obj(pairs.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.to_string(), v)).collect())
}

// ── writer ────────────────────────────────────────────────────────────────

fn write_value(out: &mut String, v: &Json, indent: Option<usize>, depth: usize) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Num(n) => write_num(out, *n),
        Json::Str(s) => write_str(out, s),
        Json::Arr(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_value(out, x, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Json::Obj(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_str(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, x, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(w) = indent {
        out.push('\n');
        for _ in 0..w * depth {
            out.push(' ');
        }
    }
}

fn write_num(out: &mut String, n: f64) {
    if !n.is_finite() {
        out.push_str("null");
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{}", n);
    }
}

pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ── parser ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ParseError(pub usize, pub &'static str);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.0, self.1)
    }
}

/// Parse a complete document (only whitespace may follow the value).
pub fn parse(s: &str) -> Result<Json, ParseError> {
    let (v, end) = parse_prefix(s)?;
    if s.as_bytes()[end..].iter().all(|b| b.is_ascii_whitespace()) {
        Ok(v)
    } else {
        Err(ParseError(end, "trailing characters"))
    }
}

/// Parse the first value; returns it and the byte offset just past it.
/// Used for the state file, where a write cut short leaves trailing bytes.
pub fn parse_prefix(s: &str) -> Result<(Json, usize), ParseError> {
    let mut p = Parser { b: s.as_bytes(), i: 0, depth: 0 };
    p.ws();
    let v = p.value()?;
    Ok((v, p.i))
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, m: &'static str) -> Result<T, ParseError> {
        Err(ParseError(self.i, m))
    }

    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if c == b' ' || c == b'\n' || c == b'\r' || c == b'\t' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.b[self.i..].starts_with(lit) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        match self.b.get(self.i) {
            None => self.err("unexpected end"),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.eat(b"true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat(b"null") => Ok(Json::Null),
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            _ => self.err("unexpected character"),
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > 256 { self.err("nested too deeply") } else { Ok(()) }
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.i += 1;
        let mut m = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            self.depth -= 1;
            return Ok(Json::Obj(m));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return self.err("expected a key");
            }
            let k = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return self.err("expected ':'");
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            m.push((k, v));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(Json::Obj(m));
                }
                _ => return self.err("expected ',' or '}'"),
            }
        }
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.i += 1;
        let mut a = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            self.depth -= 1;
            return Ok(Json::Arr(a));
        }
        loop {
            self.ws();
            a.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(Json::Arr(a));
                }
                _ => return self.err("expected ',' or ']'"),
            }
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while let Some(c) = self.b.get(self.i) {
            if c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.i += 1;
            } else {
                break;
            }
        }
        // The slice is ASCII by construction.
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
        match text.parse::<f64>() {
            Ok(n) => Ok(Json::Num(n)),
            Err(_) => Err(ParseError(start, "bad number")),
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let h = self.b.get(self.i..self.i + 4).ok_or(ParseError(self.i, "short \\u escape"))?;
        let s = std::str::from_utf8(h).map_err(|_| ParseError(self.i, "bad \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| ParseError(self.i, "bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            // Copy the run of plain bytes in one go.
            let run = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > run {
                // Input is a &str, and runs stop only at ASCII bytes, so this is valid UTF-8.
                out.push_str(std::str::from_utf8(&self.b[run..self.i]).map_err(|_| ParseError(run, "bad utf-8"))?);
            }
            match self.b.get(self.i) {
                None => return self.err("unterminated string"),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = *self.b.get(self.i).ok_or(ParseError(self.i, "unterminated escape"))?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) && self.eat(b"\\u") {
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    0xFFFD
                                }
                            } else {
                                hi
                            };
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        _ => return self.err("bad escape"),
                    }
                }
                Some(_) => return self.err("control character in string"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let src = r#"{"a":1,"b":[true,false,null,"x\"y\\z\n\u00e9\ud83d\ude00"],"c":{"d":-2.5e3,"e":""}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.get("a").u64(), Some(1));
        assert_eq!(v.at(&["c", "d"]), &Json::Num(-2500.0));
        assert_eq!(v.get("b").arr()[3].str(), Some("x\"y\\z\né😀"));
        let again = parse(&v.to_string()).unwrap();
        assert_eq!(v, again);
        assert_eq!(parse(&v.to_pretty()).unwrap(), v);
    }

    #[test]
    fn prefix_ignores_trailing_bytes() {
        let (v, end) = parse_prefix("{\"v\":3} tail-of-an-older-longer-file\"}").unwrap();
        assert_eq!(v.get("v").u64(), Some(3));
        assert_eq!(end, 7);
        assert!(parse("{\"v\":3} x").is_err());
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "{", "[1,", "{\"a\" 1}", "\"\\x\"", "tru", "{\"a\":1,}", "\"a\nb\""] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn large_strings_are_fast() {
        let big = "x".repeat(4_000_000);
        let src = format!("{{\"tool_input\":{{\"content\":\"{big}\",\"file_path\":\"/a\"}}}}");
        let t = std::time::Instant::now();
        let v = parse(&src).unwrap();
        assert_eq!(v.at(&["tool_input", "file_path"]).str(), Some("/a"));
        assert!(t.elapsed().as_millis() < 500);
    }

    #[test]
    fn set_and_object_helper() {
        let mut o = object([("a", Json::from(1u64)), ("skip", Json::Null)]);
        o.set("b", "x".into());
        o.set("a", 2u64.into());
        assert_eq!(o.to_string(), r#"{"a":2,"b":"x"}"#);
    }
}
