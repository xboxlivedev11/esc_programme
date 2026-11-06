//! Minimal JSON: a strict parser into [`Json`] (for input) and a streaming writer [`Js`] (for output).
//!
//! Input is untrusted (WebSocket frames, HTTP bodies, replicated log records), so the parser is
//! strict (RFC 8259), depth-limited and never panics. Output is written straight into a `String`
//! with no intermediate tree, which keeps the hot paths (message pushes) allocation-light.

use std::fmt::{self, Write as _};

/// A parsed JSON value. Objects keep insertion order (they are small; lookup is linear).
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

const MAX_DEPTH: usize = 64;
static EMPTY: [Json; 0] = [];

/// Parse one JSON document (surrounding whitespace allowed, trailing data rejected).
pub fn parse(s: &str) -> Result<Json, String> {
    let mut p = P { b: s.as_bytes(), i: 0, depth: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return p.err("trailing data");
    }
    Ok(v)
}

/// Parse JSON from bytes that must be UTF-8.
pub fn parse_bytes(b: &[u8]) -> Result<Json, String> {
    parse(std::str::from_utf8(b).map_err(|_| "json: invalid utf-8".to_string())?)
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl P<'_> {
    fn err<T>(&self, m: &str) -> Result<T, String> {
        Err(format!("json: {m} at byte {}", self.i))
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.b.get(self.i) {
            None => self.err("unexpected end"),
            Some(b'n') => self.lit(b"null", Json::Null),
            Some(b't') => self.lit(b"true", Json::Bool(true)),
            Some(b'f') => self.lit(b"false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => self.err("unexpected character"),
        }
    }

    fn lit(&mut self, w: &[u8], v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(w) {
            self.i += w.len();
            Ok(v)
        } else {
            self.err("bad literal")
        }
    }

    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("nested too deep");
        }
        self.i += 1; // skip '[' or '{'
        Ok(())
    }

    fn array(&mut self) -> Result<Json, String> {
        self.enter()?;
        let mut v = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            self.depth -= 1;
            return Ok(Json::Arr(v));
        }
        loop {
            v.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return self.err("expected ',' or ']'"),
            }
        }
        self.depth -= 1;
        Ok(Json::Arr(v))
    }

    fn object(&mut self) -> Result<Json, String> {
        self.enter()?;
        let mut v: Vec<(String, Json)> = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            self.depth -= 1;
            return Ok(Json::Obj(v));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return self.err("expected key");
            }
            let k = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return self.err("expected ':'");
            }
            self.i += 1;
            let val = self.value()?;
            v.push((k, val));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return self.err("expected ',' or '}'"),
            }
        }
        self.depth -= 1;
        Ok(Json::Obj(v))
    }

    fn digits(&mut self) -> usize {
        let s = self.i;
        while let Some(b'0'..=b'9') = self.b.get(self.i) {
            self.i += 1;
        }
        self.i - s
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.b[self.i] == b'-' {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return self.err("bad number"),
        }
        let mut float = false;
        if self.b.get(self.i) == Some(&b'.') {
            float = true;
            self.i += 1;
            if self.digits() == 0 {
                return self.err("bad fraction");
            }
        }
        if let Some(b'e' | b'E') = self.b.get(self.i) {
            float = true;
            self.i += 1;
            if let Some(b'+' | b'-') = self.b.get(self.i) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return self.err("bad exponent");
            }
        }
        // The slice is pure ASCII, so this cannot fail.
        let t = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("x");
        if !float && let Ok(n) = t.parse::<i64>() {
            return Ok(Json::Int(n));
        }
        match t.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Json::Num(f)),
            _ => self.err("number out of range"),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let Some(h) = self.b.get(self.i..self.i + 4) else {
            return self.err("short \\u escape");
        };
        let mut v = 0u32;
        for &c in h {
            v = v * 16
                + match c {
                    b'0'..=b'9' => (c - b'0') as u32,
                    b'a'..=b'f' => (c - b'a' + 10) as u32,
                    b'A'..=b'F' => (c - b'A' + 10) as u32,
                    _ => return self.err("bad \\u escape"),
                };
        }
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let run = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            if self.i > run {
                // Split points are ASCII bytes inside valid UTF-8, so the run is valid UTF-8.
                match std::str::from_utf8(&self.b[run..self.i]) {
                    Ok(s) => out.push_str(s),
                    Err(_) => return self.err("invalid utf-8"),
                }
            }
            match self.b.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let Some(&e) = self.b.get(self.i) else {
                        return self.err("unterminated escape");
                    };
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
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return self.err("lone high surrogate");
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return self.err("bad low surrogate");
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return self.err("lone low surrogate");
                            } else {
                                hi
                            };
                            match char::from_u32(cp) {
                                Some(ch) => out.push(ch),
                                None => return self.err("bad code point"),
                            }
                        }
                        _ => return self.err("bad escape"),
                    }
                }
                Some(_) => return self.err("control character in string"),
                None => return self.err("unterminated string"),
            }
        }
    }
}

impl Json {
    /// Member of an object (first match), or None.
    pub fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(v) => v.iter().find(|(kk, _)| kk == k).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Json::Int(n) => Some(n),
            Json::Num(f) if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 => Some(f as i64),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|n| u64::try_from(n).ok())
    }
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Json::Int(n) => Some(n as f64),
            Json::Num(f) => Some(f),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Json::Bool(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
    /// `obj[k]` as a string.
    pub fn s(&self, k: &str) -> Option<&str> {
        self.get(k)?.as_str()
    }
    /// `obj[k]` as an integer.
    pub fn i(&self, k: &str) -> Option<i64> {
        self.get(k)?.as_i64()
    }
    /// `obj[k]` as a non-negative integer.
    pub fn u(&self, k: &str) -> Option<u64> {
        self.get(k)?.as_u64()
    }
    /// `obj[k]` as a bool.
    pub fn b(&self, k: &str) -> Option<bool> {
        self.get(k)?.as_bool()
    }
    /// `obj[k]` as an array (empty when missing or not an array).
    pub fn a(&self, k: &str) -> &[Json] {
        self.get(k).and_then(Json::as_arr).unwrap_or(&EMPTY)
    }
    /// Serialize into a writer.
    pub fn write(&self, js: &mut Js) {
        match self {
            Json::Null => {
                js.null();
            }
            Json::Bool(b) => {
                js.bool(*b);
            }
            Json::Int(n) => {
                js.int(*n);
            }
            Json::Num(f) => {
                js.num(*f);
            }
            Json::Str(s) => {
                js.str(s);
            }
            Json::Arr(v) => {
                js.arr();
                for x in v {
                    x.write(js);
                }
                js.end_arr();
            }
            Json::Obj(v) => {
                js.obj();
                for (k, x) in v {
                    js.key(k);
                    x.write(js);
                }
                js.end_obj();
            }
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut js = Js::new();
        self.write(&mut js);
        f.write_str(&js.buf)
    }
}

/// Streaming JSON writer. Commas are inserted automatically: before a key or a value, a comma is
/// added unless the buffer ends with `{`, `[` or `:` (or is empty).
#[derive(Default, Debug, Clone)]
pub struct Js {
    pub buf: String,
}

impl Js {
    pub fn new() -> Self {
        Js { buf: String::with_capacity(128) }
    }
    pub fn with_capacity(n: usize) -> Self {
        Js { buf: String::with_capacity(n) }
    }
    #[inline]
    fn sep(&mut self) {
        if let Some(&c) = self.buf.as_bytes().last()
            && c != b'{'
            && c != b'['
            && c != b':'
        {
            self.buf.push(',');
        }
    }
    pub fn obj(&mut self) -> &mut Self {
        self.sep();
        self.buf.push('{');
        self
    }
    pub fn end_obj(&mut self) -> &mut Self {
        self.buf.push('}');
        self
    }
    pub fn arr(&mut self) -> &mut Self {
        self.sep();
        self.buf.push('[');
        self
    }
    pub fn end_arr(&mut self) -> &mut Self {
        self.buf.push(']');
        self
    }
    pub fn key(&mut self, k: &str) -> &mut Self {
        self.sep();
        escape_into(&mut self.buf, k);
        self.buf.push(':');
        self
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.sep();
        escape_into(&mut self.buf, v);
        self
    }
    pub fn int(&mut self, v: i64) -> &mut Self {
        self.sep();
        let _ = write!(self.buf, "{v}");
        self
    }
    pub fn uint(&mut self, v: u64) -> &mut Self {
        self.sep();
        let _ = write!(self.buf, "{v}");
        self
    }
    pub fn num(&mut self, v: f64) -> &mut Self {
        self.sep();
        if v.is_finite() {
            let _ = write!(self.buf, "{v}");
        } else {
            self.buf.push_str("null");
        }
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.sep();
        self.buf.push_str(if v { "true" } else { "false" });
        self
    }
    pub fn null(&mut self) -> &mut Self {
        self.sep();
        self.buf.push_str("null");
        self
    }
    /// Insert an already-serialized JSON value.
    pub fn raw(&mut self, json: &str) -> &mut Self {
        self.sep();
        self.buf.push_str(json);
        self
    }
    pub fn ks(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k).str(v)
    }
    pub fn ki(&mut self, k: &str, v: i64) -> &mut Self {
        self.key(k).int(v)
    }
    pub fn ku(&mut self, k: &str, v: u64) -> &mut Self {
        self.key(k).uint(v)
    }
    pub fn kb(&mut self, k: &str, v: bool) -> &mut Self {
        self.key(k).bool(v)
    }
    pub fn kn(&mut self, k: &str, v: f64) -> &mut Self {
        self.key(k).num(v)
    }
    /// Optional string: writes `null` when None.
    pub fn kos(&mut self, k: &str, v: Option<&str>) -> &mut Self {
        self.key(k);
        match v {
            Some(s) => self.str(s),
            None => self.null(),
        }
    }
    pub fn finish(self) -> String {
        self.buf
    }
}

/// Append `s` as a quoted, escaped JSON string.
pub fn escape_into(out: &mut String, s: &str) {
    out.push('"');
    let b = s.as_bytes();
    let mut start = 0;
    for (i, &c) in b.iter().enumerate() {
        let rep: &str = match c {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0..=0x1f => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if rep.is_empty() {
            let _ = write!(out, "\\u{:04x}", c);
        } else {
            out.push_str(rep);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars_and_structures() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse(" true ").unwrap(), Json::Bool(true));
        assert_eq!(parse("-0").unwrap(), Json::Int(0));
        assert_eq!(parse("9007199254740993").unwrap(), Json::Int(9_007_199_254_740_993));
        assert_eq!(parse("1.5e3").unwrap(), Json::Num(1500.0));
        assert_eq!(parse("99999999999999999999").unwrap(), Json::Num(1e20));
        let v = parse(r#"{"a":[1,2,{"b":"c"}],"d":{}, "e":[]}"#).unwrap();
        assert_eq!(v.a("a").len(), 3);
        assert_eq!(v.a("a")[2].s("b"), Some("c"));
        assert_eq!(v.get("d"), Some(&Json::Obj(vec![])));
        assert_eq!(v.u("missing"), None);
    }

    #[test]
    fn parses_strings_and_escapes() {
        let v = parse(r#""a\"b\\c\/d\b\f\n\r\t\u00e9\ud83d\ude00 ok""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"b\\c/d\u{8}\u{c}\n\r\té😀 ok"));
        assert_eq!(parse("\"ação ✓\"").unwrap().as_str(), Some("ação ✓"));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "", "nul", "[1,]", "{\"a\"1}", "{\"a\":1,}", "01", "1.", ".5", "1e", "\"\\x\"", "\"a",
            "\"\\ud800\"", "\"\\udc00\"", "\"\u{1}\"", "[1] x", "{a:1}", "1e999", "--1", "+1",
        ] {
            assert!(parse(bad).is_err(), "should reject {bad:?}");
        }
        let deep = "[".repeat(65) + &"]".repeat(65);
        assert!(parse(&deep).is_err());
        let ok = "[".repeat(64) + &"]".repeat(64);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn writer_inserts_commas_and_escapes() {
        let mut j = Js::new();
        j.obj().ks("a", "x\"y\n").ki("b", -3).key("c").arr().int(1).str("s").obj().end_obj().null().end_arr();
        j.kb("d", true).kn("e", 0.25).kn("f", f64::NAN).kos("g", None).end_obj();
        assert_eq!(j.buf, r#"{"a":"x\"y\n","b":-3,"c":[1,"s",{},null],"d":true,"e":0.25,"f":null,"g":null}"#);
        let mut c = String::new();
        escape_into(&mut c, "\u{1}é");
        assert_eq!(c, "\"\\u0001é\"");
    }

    #[test]
    fn round_trips() {
        let src = r#"{"k":[true,false,null,-12,3.5,"t\u0000x",{"z":[]}],"u":"😀"}"#;
        let v = parse(src).unwrap();
        let again = parse(&v.to_string()).unwrap();
        assert_eq!(v, again);
    }
}
