//! A tiny dependency-free JSON parser.
//!
//! phi-plan deliberately keeps zero runtime dependencies, and the only JSON
//! the extension ever sees is the tool-argument payload the host forwards
//! (`update_plan` args). A full DOM is overkill, but a purpose-built string
//! scanner is brittle against escaped quotes/unicode, so this is a small
//! recursive-descent parser covering the JSON subset we need: null / bool /
//! number / string / array / object. Everything else is ignored at call
//! sites, never echoed back verbatim, so losses are acceptable.

use std::fmt;

/// Parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// Looks up a field on an object value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }
}

/// Parses `input` as a single JSON value; trailing non-whitespace is an
/// error. Returns a human-readable message on malformed input.
pub fn parse(input: &[u8]) -> Result<Value, String> {
    let mut p = Parser { b: input, i: 0 };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, want: u8) -> Result<(), String> {
        match self.bump() {
            Some(c) if c == want => Ok(()),
            Some(c) => Err(format!(
                "expected `{}`, found `{}` at byte {}",
                want as char,
                c as char,
                self.i - 1
            )),
            None => Err(format!("expected `{}`, found end of input", want as char)),
        }
    }

    fn lit(&mut self, word: &[u8]) -> Result<(), String> {
        if self.b.len() - self.i < word.len() || &self.b[self.i..self.i + word.len()] != word {
            return Err(format!("invalid literal at byte {}", self.i));
        }
        self.i += word.len();
        Ok(())
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            None => Err("unexpected end of input".into()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => {
                self.lit(b"true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.lit(b"false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.lit(b"null")?;
                Ok(Value::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(c) => Err(format!(
                "unexpected character `{}` at byte {}",
                c as char, self.i
            )),
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-' {
                self.i += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i])
            .map_err(|_| format!("invalid number at byte {}", start))?;
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| format!("invalid number `{text}` at byte {start}"))
    }

    /// Parses a `"..."` string (opening quote already consumed by caller
    /// only via `string()` itself), decoding JSON escapes.
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = match self.bump() {
                Some(c) => c,
                None => return Err("unterminated string".into()),
            };
            match c {
                b'"' => break,
                b'\\' => {
                    let esc = self
                        .bump()
                        .ok_or_else(|| "unterminated escape".to_string())?;
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            // Combine surrogate pairs; a lone surrogate is
                            // replaced so parsing never fails on it.
                            let cp = if (0xD800..=0xDBFF).contains(&hi)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..=0xDFFF).contains(&lo) {
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    0xFFFD
                                }
                            } else if (0xD800..=0xDFFF).contains(&hi) {
                                0xFFFD
                            } else {
                                hi
                            };
                            push_codepoint(&mut out, cp);
                        }
                        other => return Err(format!("invalid escape `\\{}`", other as char)),
                    }
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).map_err(|_| "string is not valid UTF-8".to_string())
    }

    /// Reads exactly four hex digits after a `\u`.
    fn hex4(&mut self) -> Result<u32, String> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self
                .bump()
                .ok_or_else(|| "truncated \\u escape".to_string())?;
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return Err("invalid \\u escape".into()),
            };
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn object(&mut self) -> Result<Value, String> {
        self.expect(b'{')?;
        self.skip_ws();
        let mut fields = Vec::new();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let v = self.value()?;
            fields.push((key, v));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                Some(c) => return Err(format!("expected `,` or `}}`, found `{}`", c as char)),
                None => return Err("unterminated object".into()),
            }
        }
        Ok(Value::Obj(fields))
    }

    fn array(&mut self) -> Result<Value, String> {
        self.expect(b'[')?;
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.skip_ws();
            let v = self.value()?;
            items.push(v);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                Some(c) => return Err(format!("expected `,` or `]`, found `{}`", c as char)),
                None => return Err("unterminated array".into()),
            }
        }
        Ok(Value::Arr(items))
    }
}

/// Appends a Unicode codepoint as UTF-8.
fn push_codepoint(out: &mut Vec<u8>, cp: u32) {
    if let Some(c) = char::from_u32(cp) {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(n) => write!(f, "{n}"),
            Value::Str(s) => write!(f, "\"{s}\""),
            Value::Arr(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Obj(fields) => {
                write!(f, "{{")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "\"{k}\": {v}")?;
                }
                write!(f, "}}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(input: &str) -> Value {
        parse(input.as_bytes()).expect("parse should succeed")
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(s("null"), Value::Null);
        assert_eq!(s("true"), Value::Bool(true));
        assert_eq!(s("  false  "), Value::Bool(false));
        assert_eq!(s("-12.5e2"), Value::Num(-1250.0));
    }

    #[test]
    fn parses_escapes_and_unicode() {
        let v = s(r#""a\"b\\c\/d\n\t\u00e9\ud83d\ude00""#);
        assert_eq!(v, Value::Str("a\"b\\c/d\n\té😀".to_string()));
    }

    #[test]
    fn parses_nested() {
        let v = s(r#"{"plan":[{"content":"step \"one\"","status":"done"},{"content":"two"}]}"#);
        let plan = v.get("plan").unwrap().as_array().unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].get("content").unwrap().as_str().unwrap(),
            "step \"one\""
        );
        assert_eq!(plan[0].get("status").unwrap().as_str().unwrap(), "done");
        assert_eq!(plan[1].get("notes"), None);
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["", "{", "[1,", r#"{"a" 1}"#, "tru", r#""unterminated"#] {
            assert!(parse(bad.as_bytes()).is_err(), "should reject: {bad}");
        }
    }
}
