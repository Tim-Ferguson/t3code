//! jsonc-parser object construction: __proto__ assignment affects lookup, not own entries.
use regex::Regex;
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::sync::LazyLock;
pub(super) enum Node {
    Null,
    Bool(bool),
    Number(Option<Number>),
    String(String),
    Array(Vec<Node>),
    Object {
        own: BTreeMap<String, Node>,
        prototype: Option<Box<Node>>,
        default_prototype: bool,
    },
}
impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Self::Object { own, prototype, .. } => own
                .get(key)
                .or_else(|| prototype.as_deref().and_then(|v| v.get(key))),
            _ => None,
        }
    }
    pub fn string(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
    pub fn own(&self) -> Option<&BTreeMap<String, Node>> {
        match self {
            Self::Object { own, .. } => Some(own),
            _ => None,
        }
    }
    pub fn array(&self) -> Option<&[Node]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }
    fn setter(&self) -> bool {
        match self {
            Self::Object {
                own,
                prototype,
                default_prototype,
            } => {
                !own.contains_key("__proto__")
                    && prototype
                        .as_deref()
                        .map_or(*default_prototype, Node::setter)
            }
            Self::Array(_) => true,
            _ => false,
        }
    }
    pub fn wire(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(v) => Value::Bool(*v),
            Self::Number(v) => v.clone().map_or(Value::Null, Value::Number),
            Self::String(v) => Value::String(v.clone()),
            Self::Array(v) => Value::Array(v.iter().map(Node::wire).collect()),
            Self::Object { own, .. } => Value::Object(
                own.iter()
                    .map(|(k, v)| (k.clone(), v.wire()))
                    .collect::<Map<_, _>>(),
            ),
        }
    }
}
struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
    depth: usize,
}
impl Parser<'_> {
    fn skip(&mut self) -> Result<(), ()> {
        loop {
            while self
                .bytes
                .get(self.position)
                .is_some_and(|v| matches!(v, b' ' | b'\t' | b'\r' | b'\n'))
            {
                self.position += 1;
            }
            if self.bytes.get(self.position..self.position + 2) == Some(b"//") {
                self.position += 2;
                while self
                    .bytes
                    .get(self.position)
                    .is_some_and(|v| !matches!(v, b'\r' | b'\n'))
                {
                    self.position += 1;
                }
            } else if self.bytes.get(self.position..self.position + 2) == Some(b"/*") {
                self.position += 2;
                let rest = &self.bytes[self.position..];
                let end = rest.windows(2).position(|v| v == b"*/").ok_or(())?;
                self.position += end + 2;
            } else {
                return Ok(());
            }
        }
    }
    fn string(&mut self) -> Result<String, ()> {
        if self.bytes.get(self.position) != Some(&b'"') {
            return Err(());
        }
        let start = self.position;
        self.position += 1;
        let mut escaped = false;
        while let Some(&b) = self.bytes.get(self.position) {
            self.position += 1;
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                return serde_json::from_slice(&self.bytes[start..self.position]).map_err(|_| ());
            }
        }
        Err(())
    }
    fn value(&mut self) -> Result<Node, ()> {
        self.skip()?;
        self.depth += 1;
        // Reject adversarial nesting before a native/WASM stack overflow. Package
        // include depth is separately governed by the original eight-file limit.
        if self.depth > 512 {
            return Err(());
        }
        let result = self.inner();
        self.depth -= 1;
        result
    }
    fn inner(&mut self) -> Result<Node, ()> {
        match self.bytes.get(self.position).ok_or(())? {
            b'"' => self.string().map(Node::String),
            b'{' => {
                self.position += 1;
                let mut own = BTreeMap::new();
                let mut prototype = None::<Box<Node>>;
                let mut default = true;
                self.skip()?;
                if self.bytes.get(self.position) == Some(&b'}') {
                    self.position += 1;
                    return Ok(Node::Object {
                        own,
                        prototype,
                        default_prototype: default,
                    });
                }
                loop {
                    self.skip()?;
                    let key = self.string()?;
                    self.skip()?;
                    if self.bytes.get(self.position) != Some(&b':') {
                        return Err(());
                    }
                    self.position += 1;
                    let value = self.value()?;
                    if key == "__proto__"
                        && !own.contains_key(&key)
                        && prototype.as_deref().map_or(default, Node::setter)
                    {
                        match value {
                            Node::Null => {
                                prototype = None;
                                default = false;
                            }
                            Node::Object { .. } | Node::Array(_) => {
                                prototype = Some(Box::new(value));
                                default = false;
                            }
                            _ => {}
                        }
                    } else {
                        own.insert(key, value);
                    }
                    self.skip()?;
                    match self.bytes.get(self.position) {
                        Some(b'}') => {
                            self.position += 1;
                            break;
                        }
                        Some(b',') => {
                            self.position += 1;
                            self.skip()?;
                            if self.bytes.get(self.position) == Some(&b'}') {
                                self.position += 1;
                                break;
                            }
                        }
                        _ => return Err(()),
                    }
                }
                Ok(Node::Object {
                    own,
                    prototype,
                    default_prototype: default,
                })
            }
            b'[' => {
                self.position += 1;
                let mut values = Vec::new();
                self.skip()?;
                if self.bytes.get(self.position) == Some(&b']') {
                    self.position += 1;
                    return Ok(Node::Array(values));
                }
                loop {
                    values.push(self.value()?);
                    self.skip()?;
                    match self.bytes.get(self.position) {
                        Some(b']') => {
                            self.position += 1;
                            break;
                        }
                        Some(b',') => {
                            self.position += 1;
                            self.skip()?;
                            if self.bytes.get(self.position) == Some(&b']') {
                                self.position += 1;
                                break;
                            }
                        }
                        _ => return Err(()),
                    }
                }
                Ok(Node::Array(values))
            }
            b't' | b'f' | b'n' => {
                for (literal, value) in [
                    (b"true".as_slice(), Node::Bool(true)),
                    (b"false".as_slice(), Node::Bool(false)),
                    (b"null".as_slice(), Node::Null),
                ] {
                    if self.bytes.get(self.position..self.position + literal.len()) == Some(literal)
                    {
                        self.position += literal.len();
                        return Ok(value);
                    }
                }
                Err(())
            }
            b'-' | b'0'..=b'9' => {
                static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
                    Regex::new(r"^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?$").unwrap()
                });
                let start = self.position;
                while self.bytes.get(self.position).is_some_and(|v| {
                    v.is_ascii_digit() || matches!(v, b'-' | b'+' | b'.' | b'e' | b'E')
                }) {
                    self.position += 1;
                }
                let token =
                    std::str::from_utf8(&self.bytes[start..self.position]).map_err(|_| ())?;
                if !NUMBER.is_match(token) {
                    return Err(());
                }
                let number = token.parse::<f64>().map_err(|_| ())?;
                Ok(Node::Number(if number.is_finite() {
                    Some(serde_json::from_str::<Number>(token).map_err(|_| ())?)
                } else {
                    None
                }))
            }
            _ => Err(()),
        }
    }
}
pub(super) fn parse(source: &str) -> Result<Node, ()> {
    let mut parser = Parser {
        bytes: source.as_bytes(),
        position: 0,
        depth: 0,
    };
    let value = parser.value()?;
    parser.skip()?;
    if parser.position != parser.bytes.len() || value.own().is_none() {
        return Err(());
    }
    Ok(value)
}
