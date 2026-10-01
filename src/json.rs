//! A small JSON reader and writer, because this crate has no dependencies.
//!
//! **Why hand-rolled.** Three reasons, in the order they matter. This binary is pulled into a
//! container image by checksum and runs on the ssh path, so every dependency is a thing to
//! audit and a thing that has to cross-compile to static musl. Hook payloads must be read
//! *tolerantly* — Claude Code updates itself in place in these containers, so an unknown field
//! or a renamed one has to degrade to "no evidence" rather than to an error on the door — and
//! a `Value` tree does that more honestly than a struct with twelve `Option`s. And the
//! development container this is written in has no C linker at all, so a crate with a build
//! script or a proc macro cannot be compiled here; a zero-dependency crate builds and tests
//! locally against `aarch64-unknown-linux-musl` with the toolchain's own `rust-lld`.
//!
//! It is deliberately small: enough for the registry's own files and for reading a hook
//! payload. Not a general-purpose library, and it says so rather than growing into one.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    /// Sorted by key, so a record written twice with the same content is byte-identical and a
    /// diff of the registry means something changed.
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn obj() -> Value {
        Value::Obj(BTreeMap::new())
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(key),
            _ => None,
        }
    }
    pub fn set(&mut self, key: &str, v: Value) {
        if let Value::Obj(m) = self {
            m.insert(key.to_string(), v);
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    /// Numbers come back as f64 because that is what JSON has. Milliseconds since the epoch
    /// are exact in an f64 until the year 287396, which is far enough.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Num(n) if *n >= 0.0 => Some(*n as u64),
            _ => None,
        }
    }
    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().and_then(|n| u32::try_from(n).ok())
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn string(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }
    pub fn num(n: impl Into<f64>) -> Value {
        Value::Num(n.into())
    }
}

// ── writing ─────────────────────────────────────────────────────────────────

pub fn to_string_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v, 0);
    out.push('\n');
    out
}

fn write_value(out: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => {
            // An integral f64 is written without a decimal point, so a millisecond timestamp
            // reads as a timestamp rather than as 1.7593e12.
            if n.fract() == 0.0 && n.abs() < 9e15 {
                let _ = write!(out, "{}", *n as i64);
            } else {
                let _ = write!(out, "{n}");
            }
        }
        Value::Str(s) => write_str(out, s),
        Value::Arr(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in a.iter().enumerate() {
                pad(out, indent + 1);
                write_value(out, item, indent + 1);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, indent);
            out.push(']');
        }
        Value::Obj(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (k, val)) in m.iter().enumerate() {
                pad(out, indent + 1);
                write_str(out, k);
                out.push_str(": ");
                write_value(out, val, indent + 1);
                if i + 1 < m.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, indent);
            out.push('}');
        }
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Everything below 0x20 must be escaped or the output is not JSON. A session
            // title is free text and has reached us with control bytes in it before.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ── reading ─────────────────────────────────────────────────────────────────

pub fn parse(s: &str) -> Result<Value, String> {
    let mut p = Parser {
        b: s.as_bytes(),
        i: 0,
        src: s,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing input at byte {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    src: &'a str,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!(
                "expected {:?} at byte {}, found {:?}",
                c as char,
                self.i,
                self.peek().map(|b| b as char)
            ))
        }
    }
    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'n') => self.lit("null", Value::Null),
            Some(_) => self.number(),
            None => Err("unexpected end of input".into()),
        }
    }
    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        if self.src[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.i))
        }
    }
    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.i += 1;
            } else {
                break;
            }
        }
        self.src[start..self.i]
            .parse::<f64>()
            .map(Value::Num)
            .map_err(|e| format!("bad number at byte {start}: {e}"))
    }
    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
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
                        b'u' => out.push(self.unicode()?),
                        other => return Err(format!("bad escape \\{}", other as char)),
                    }
                }
                _ => {
                    // Walk back and take the whole UTF-8 character: a title can be any text,
                    // and splitting a multi-byte character here would produce invalid UTF-8.
                    self.i -= 1;
                    let rest = &self.src[self.i..];
                    let ch = rest.chars().next().ok_or("invalid utf-8")?;
                    self.i += ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }
    /// A `\uXXXX` escape, including the surrogate pair that any character above the BMP
    /// arrives as — an emoji in a session title is exactly that.
    fn unicode(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        if (0xD800..0xDC00).contains(&hi) {
            if self.peek() == Some(b'\\') {
                self.i += 1;
                self.eat(b'u')?;
                let lo = self.hex4()?;
                if (0xDC00..0xE000).contains(&lo) {
                    let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    return char::from_u32(c).ok_or_else(|| "bad surrogate pair".into());
                }
            }
            // A lone surrogate is not a character. Replacement rather than an error: the
            // payload is still readable and refusing it would lose the whole event.
            return Ok('\u{FFFD}');
        }
        char::from_u32(hi).ok_or_else(|| "bad \\u escape".into())
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let s = self
            .src
            .get(self.i..self.i + 4)
            .ok_or("truncated \\u escape")?;
        self.i += 4;
        u32::from_str_radix(s, 16).map_err(|e| format!("bad hex in \\u escape: {e}"))
    }
    fn array(&mut self) -> Result<Value, String> {
        self.eat(b'[')?;
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Arr(out));
                }
                _ => return Err(format!("expected , or ] at byte {}", self.i)),
            }
        }
    }
    fn object(&mut self) -> Result<Value, String> {
        self.eat(b'{')?;
        let mut m = BTreeMap::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Obj(m));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.eat(b':')?;
            self.ws();
            let v = self.value()?;
            m.insert(k, v);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Obj(m));
                }
                _ => return Err(format!("expected , or }} at byte {}", self.i)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_record_shaped_object() {
        let src = r#"{"a":1,"b":"two","c":[true,null,3.5],"d":{"e":{}}}"#;
        let v = parse(src).expect("parses");
        let again = parse(&to_string_pretty(&v)).expect("re-parses what it wrote");
        assert_eq!(v, again);
    }

    #[test]
    fn integral_numbers_are_not_written_in_exponent_form() {
        // A millisecond timestamp has to read as one, or the registry is unreadable by a
        // person and that is most of what it is for.
        let v = Value::num(1_759_300_000_000_f64);
        assert_eq!(to_string_pretty(&v).trim(), "1759300000000");
    }

    #[test]
    fn titles_survive_escapes_emoji_and_control_bytes() {
        let nasty = "a \"quoted\" \\ path\nnewline\t\u{1F600} \u{1}";
        let mut o = Value::obj();
        o.set("title", Value::string(nasty));
        let back = parse(&to_string_pretty(&o)).expect("parses");
        assert_eq!(back.get("title").and_then(|v| v.as_str()), Some(nasty));
    }

    #[test]
    fn reads_a_surrogate_pair_as_one_character() {
        let v = parse(r#"{"t":"😀"}"#).expect("parses");
        assert_eq!(v.get("t").and_then(|v| v.as_str()), Some("\u{1F600}"));
    }

    #[test]
    fn a_lone_surrogate_does_not_lose_the_whole_payload() {
        let v = parse(r#"{"t":"\ud83d"}"#).expect("parses");
        assert_eq!(v.get("t").and_then(|v| v.as_str()), Some("\u{FFFD}"));
    }

    #[test]
    fn rejects_what_is_not_json() {
        for bad in [r#"{"a":}"#, "{", r#"{"a":1}}"#, "", r#"{"a" 1}"#] {
            assert!(parse(bad).is_err(), "should have refused {bad:?}");
        }
    }

    #[test]
    fn unknown_fields_are_simply_present() {
        // The tolerance the hook relies on: a payload with a field this version has never
        // heard of parses, and reading a field that is absent is None rather than an error.
        let v = parse(r#"{"hook_event_name":"Stop","brand_new_field":{"x":1}}"#).unwrap();
        assert_eq!(
            v.get("hook_event_name").and_then(|v| v.as_str()),
            Some("Stop")
        );
        assert!(v.get("not_there").is_none());
    }
}
