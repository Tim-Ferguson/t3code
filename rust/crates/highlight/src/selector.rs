//! TextMate injection selectors use an ASCII tokenizer and scope subsequences.
#[derive(Clone, Debug)]
enum Expression { Names(Vec<String>), Not(Option<Box<Self>>), All(Vec<Self>), Any(Vec<Self>) }
#[derive(Clone, Debug)]
pub struct Selector { pub priority: i8, expression: Expression }
impl Selector {
    pub fn parse(source: &str) -> Vec<Self> {
        let mut tokens = Vec::new();
        let mut offset = 0;
        let bytes = source.as_bytes();
        while offset < bytes.len() {
            if (bytes[offset] == b'L' || bytes[offset] == b'R') && bytes.get(offset+1) == Some(&b':') {
                tokens.push(source[offset..offset+2].to_owned());offset+=2;
            } else if identifier(bytes[offset]) {
                let start=offset;offset+=1;
                while bytes.get(offset).is_some_and(|byte| identifier(*byte) || *byte == b'-') { offset+=1; }
                tokens.push(source[start..offset].to_owned());
            } else if b",|-()".contains(&bytes[offset]) { tokens.push((bytes[offset] as char).to_string());offset+=1; }
            else { offset+=1; }
        }
        let mut parser = Parser { tokens, index: 0 };
        let mut result = Vec::new();
        while let Some(token) = parser.peek() {
            let priority = match token { "L:" => {parser.index+=1;-1}, "R:" => {parser.index+=1;1}, _=>0 };
            result.push(Self { priority, expression: parser.conjunction() });
            if parser.peek() != Some(",") { break; }parser.index+=1;
        }
        result
    }
    pub fn matches(&self, scopes: &[String]) -> bool { self.expression.matches(scopes) }
}
fn identifier(byte: u8) -> bool { byte.is_ascii_alphanumeric() || b"_.:".contains(&byte) }
impl Expression {
    fn matches(&self, scopes: &[String]) -> bool {
        match self {
            Self::All(items) => items.iter().all(|item| item.matches(scopes)),
            Self::Any(items) => items.iter().any(|item| item.matches(scopes)),
            Self::Not(item) => item.as_ref().is_some_and(|item| !item.matches(scopes)),
            Self::Names(names) => {
                let mut at=0;
                for name in names {
                    let Some(found) = scopes[at..].iter().position(|scope| scope == name
                        || scope.strip_prefix(name.as_str()).is_some_and(|suffix| suffix.starts_with('.'))) else { return false; };
                    at+=found+1;
                }
                true
            }
        }
    }
}
struct Parser { tokens: Vec<String>, index: usize }
impl Parser {
    fn peek(&self) -> Option<&str> { self.tokens.get(self.index).map(String::as_str) }
    fn operand(&mut self) -> Option<Expression> {
        match self.peek()? {
            "-" => { self.index+=1;Some(Expression::Not(self.operand().map(Box::new))) }
            "(" => {
                self.index+=1;let mut items=vec![self.conjunction()];
                while matches!(self.peek(),Some("|"|",")) {
                    while matches!(self.peek(),Some("|"|",")) { self.index+=1; }
                    items.push(self.conjunction());
                }
                if self.peek() == Some(")") { self.index+=1; }
                Some(Expression::Any(items))
            }
            token if token.bytes().any(identifier) => {
                let mut names=Vec::new();
                while self.peek().is_some_and(|token| token.bytes().any(identifier)) {
                    names.push(self.tokens[self.index].clone());self.index+=1;
                }
                Some(Expression::Names(names))
            }
            _ => None,
        }
    }
    fn conjunction(&mut self) -> Expression {
        let mut items=Vec::new();while let Some(item)=self.operand() { items.push(item); }Expression::All(items)
    }
}
