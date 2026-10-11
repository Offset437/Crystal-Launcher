//! A small, strict JSON reader that borrows from its input. String values are
//! kept as raw slices, so secret fields can be base64-decoded straight out of the
//! (zeroized) line buffer without being copied anywhere first.

use std::fmt;

const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    /// The literal number text, validated against the JSON grammar.
    Number(&'a str),
    Str(JStr<'a>),
    Array(Vec<Value<'a>>),
    Object(Vec<(JStr<'a>, Value<'a>)>),
}

/// A string's raw bytes between the quotes, escapes not yet processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JStr<'a> {
    raw: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub offset: usize,
    pub what: &'static str,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately no input excerpt: the input may hold secrets.
        write!(f, "invalid JSON at byte {}: {}", self.offset, self.what)
    }
}

impl<'a> JStr<'a> {
    /// The bytes, if the string has no escapes.
    pub fn plain(&self) -> Option<&'a [u8]> {
        (!self.raw.contains(&b'\\')).then_some(self.raw)
    }

    pub fn eq_str(&self, s: &str) -> bool {
        self.plain() == Some(s.as_bytes())
    }

    /// Decodes escapes. Use only for non-secret values.
    pub fn decode(&self) -> Result<String, Error> {
        let err = |what| Error { offset: 0, what };
        let mut out = String::with_capacity(self.raw.len());
        let s = std::str::from_utf8(self.raw).map_err(|_| err("string is not UTF-8"))?;
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next().ok_or(err("dangling escape"))? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hi = hex4(&mut chars).ok_or(err("bad \\u escape"))?;
                    let cp = if (0xD800..0xDC00).contains(&hi) {
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            return Err(err("unpaired surrogate"));
                        }
                        let lo = hex4(&mut chars).ok_or(err("bad \\u escape"))?;
                        if !(0xDC00..0xE000).contains(&lo) {
                            return Err(err("unpaired surrogate"));
                        }
                        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                    } else {
                        hi
                    };
                    out.push(char::from_u32(cp).ok_or(err("bad code point"))?);
                }
                _ => return Err(err("unknown escape")),
            }
        }
        Ok(out)
    }
}

fn hex4(chars: &mut std::str::Chars<'_>) -> Option<u32> {
    let mut v = 0;
    for _ in 0..4 {
        v = v * 16 + chars.next()?.to_digit(16)?;
    }
    Some(v)
}

impl<'a> Value<'a> {
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        match self {
            Value::Object(fields) => fields.iter().rev().find(|(k, _)| k.eq_str(key)).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.parse().ok(),
            _ => None,
        }
    }

    pub fn as_jstr(&self) -> Option<JStr<'a>> {
        match self {
            Value::Str(s) => Some(*s),
            _ => None,
        }
    }
}

pub fn parse(input: &[u8]) -> Result<Value<'_>, Error> {
    let mut p = Parser { s: input, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.s.len() {
        return Err(p.err("trailing data"));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, what: &'static str) -> Error {
        Error { offset: self.i, what }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), Error> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(self.err("unexpected character"))
        }
    }

    fn lit(&mut self, word: &'static [u8], v: Value<'a>) -> Result<Value<'a>, Error> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("bad literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deeply"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'n') => self.lit(b"null", Value::Null),
            Some(b't') => self.lit(b"true", Value::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Bool(false)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("expected a value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        self.eat(b'{')?;
        let mut fields = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Object(fields));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.eat(b':')?;
            self.ws();
            let v = self.value(depth + 1)?;
            fields.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(fields));
                }
                _ => return Err(self.err("expected , or }")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.err("expected , or ]")),
            }
        }
    }

    fn string(&mut self) -> Result<JStr<'a>, Error> {
        self.eat(b'"')?;
        let start = self.i;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => self.i += 2,
                Some(c) if c < 0x20 => return Err(self.err("control character in string")),
                Some(_) => self.i += 1,
            }
        }
        if self.i > self.s.len() {
            return Err(self.err("unterminated string"));
        }
        let raw = &self.s[start..self.i];
        self.i += 1;
        Ok(JStr { raw })
    }

    fn number(&mut self) -> Result<Value<'a>, Error> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.err("bad number")),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("bad number"));
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("bad number"));
            }
            self.digits();
        }
        // The grammar above only admits ASCII.
        Ok(Value::Number(std::str::from_utf8(&self.s[start..self.i]).unwrap_or("")))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_values() {
        let bs = '\\';
        let text = format!(r#" {{"a":1,"b":[true,false,null,-2.5e3],"c":{{"d":"x\"y"}},"e":"{bs}u00e9{bs}ud83d{bs}ude00"}} "#);
        let v = parse(text.as_bytes()).unwrap();
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(1));
        assert_eq!(v.get("c").unwrap().get("d").unwrap().as_jstr().unwrap().decode().unwrap(), "x\"y");
        assert_eq!(v.get("e").unwrap().as_jstr().unwrap().decode().unwrap(), "\u{e9}\u{1F600}");
        assert!(v.get("c").unwrap().get("d").unwrap().as_jstr().unwrap().plain().is_none());
    }

    #[test]
    fn rejects_malformed() {
        for bad in [&b"{"[..], b"{\"a\" 1}", b"{\"a\":1,}", b"[1 2]", b"\"abc", b"01", b"1.", b"{} x", b"\"a\x01\"", b"nul", b"\"\\"] {
            assert!(parse(bad).is_err(), "{:?}", std::str::from_utf8(bad));
        }
        let deep = "[".repeat(40) + &"]".repeat(40);
        assert!(parse(deep.as_bytes()).is_err());
    }
}
