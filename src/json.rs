// Minimal RFC 8259 JSON reader + writer helpers. std-only on purpose: the whole
// crate carries exactly one external dependency (rusqlite) because the spec
// requires SQLite. Parsing the engine's /metrics by hand is correctness-critical
// for session windows, so the parser is a real recursive-descent one, not regex.

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, k: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.iter().find(|(kk, _)| kk == k).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn path(&self, keys: &[&str]) -> Option<&Value> {
        let mut cur = self;
        for k in keys {
            cur = cur.get(k)?;
        }
        Some(cur)
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64().map(|f| f as i64)
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64().map(|f| f.max(0.0) as u64)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn lit(&mut self, s: &[u8], v: Value) -> Result<Value, String> {
        if self.b.len() >= self.i + s.len() && &self.b[self.i..self.i + s.len()] == s {
            self.i += s.len();
            Ok(v)
        } else {
            Err(format!("bad literal at {}", self.i))
        }
    }
    fn string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err(format!("expected string at {}", self.i));
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = *self.b.get(self.i).ok_or("eof in string")?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("eof in escape")?;
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
                            let hex = self
                                .b
                                .get(self.i..self.i + 4)
                                .ok_or("eof in \\u")?;
                            let s = std::str::from_utf8(hex).map_err(|e| e.to_string())?;
                            let cp = u32::from_str_radix(s, 16).map_err(|e| e.to_string())?;
                            self.i += 4;
                            let mut cp = cp;
                            // surrogate pair
                            if (0xD800..0xDC00).contains(&cp)
                                && self.b.get(self.i) == Some(&b'\\')
                                && self.b.get(self.i + 1) == Some(&b'u')
                            {
                                if let Some(hex2) = self.b.get(self.i + 2..self.i + 6) {
                                    if let Ok(s2) = std::str::from_utf8(hex2) {
                                        if let Ok(lo) = u32::from_str_radix(s2, 16) {
                                            cp = 0x10000
                                                + ((cp - 0xD800) << 10)
                                                + (lo.wrapping_sub(0xDC00) & 0x3FF);
                                            self.i += 6;
                                        }
                                    }
                                }
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
                _ => {
                    // collect raw utf8 bytes
                    let start = self.i - 1;
                    let mut end = self.i;
                    while end < self.b.len() && !matches!(self.b[end], b'"' | b'\\') {
                        end += 1;
                    }
                    let chunk = std::str::from_utf8(&self.b[start..end]).map_err(|e| e.to_string())?;
                    out.push_str(chunk);
                    self.i = end;
                }
            }
        }
    }
    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|e| e.to_string())?;
        s.parse::<f64>()
            .map(Value::Num)
            .map_err(|e| format!("bad number {s}: {e}"))
    }
    fn value(&mut self) -> Result<Value, String> {
        self.ws();
        match self.peek().ok_or("eof")? {
            b'{' => {
                self.i += 1;
                let mut m = Vec::new();
                loop {
                    self.ws();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        return Ok(Value::Obj(m));
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return Err(format!("expected ':' at {}", self.i));
                    }
                    self.i += 1;
                    let v = self.value()?;
                    m.push((k, v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Obj(m));
                        }
                        _ => return Err(format!("expected , or }} at {}", self.i)),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                loop {
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        return Ok(Value::Arr(a));
                    }
                    a.push(self.value()?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::Arr(a));
                        }
                        _ => return Err(format!("expected , or ] at {}", self.i)),
                    }
                }
            }
            b'"' => Ok(Value::Str(self.string()?)),
            b't' => self.lit(b"true", Value::Bool(true)),
            b'f' => self.lit(b"false", Value::Bool(false)),
            b'n' => self.lit(b"null", Value::Null),
            _ => self.number(),
        }
    }
}

pub fn parse(s: &[u8]) -> Result<Value, String> {
    let mut p = P { b: s, i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != s.len() {
        return Err(format!("trailing bytes at {}", p.i));
    }
    Ok(v)
}

/// Escape a string for emission inside JSON double quotes.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Render a Value back to compact JSON (used to pass parsed engine blocks through).
pub fn render(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Num(n) => {
            if n.is_finite() {
                if n.fract() == 0.0 && n.abs() < 9.0e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{}", n)
                }
            } else {
                "null".into()
            }
        }
        Value::Str(s) => format!("\"{}\"", esc(s)),
        Value::Arr(a) => format!("[{}]", a.iter().map(render).collect::<Vec<_>>().join(",")),
        Value::Obj(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("\"{}\":{}", esc(k), render(v)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

#[cfg(test)]
mod t {
    use super::*;
    #[test]
    fn basic() {
        let v = parse(br#"{"a":1,"b":[1,2,{"c":"x\ny"}],"d":null,"e":true}"#).unwrap();
        assert_eq!(v.path(&["a"]).unwrap().as_i64(), Some(1));
        assert_eq!(v.path(&["b", "2", "c"]).is_some(), false);
        assert_eq!(v.get("b").unwrap().as_f64().is_some(), false);
        assert_eq!(v.get("e").unwrap().as_bool(), Some(true));
    }
}
