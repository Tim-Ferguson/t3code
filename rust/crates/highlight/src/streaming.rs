use crate::{scanner::{Engine,Error},tokenizer::{Line,State,Tokenizer}};
use std::rc::Rc;

/// Completed lines and their grammar stack are committed once. An unfinished
/// line is always parsed from that committed stack; completing a delimiter must
/// not accidentally advance the state twice or replace preceding line nodes.
pub struct Streaming<E: Engine> {
    tokenizer: Tokenizer<E>,
    prefix: String,
    completed: Vec<Rc<Line>>,
    state: Option<State>,
    pub parsed_lines: usize,
}
impl<E: Engine> Streaming<E> {
    pub fn new(tokenizer: Tokenizer<E>) -> Self {
        Self { tokenizer,prefix:String::new(),completed:Vec::new(),state:None,parsed_lines:0 }
    }
    pub fn document(&mut self, code: &str) -> Result<Vec<Rc<Line>>,Error> {
        if code.contains('\r') {
            // The source does not cache a CR which may become a CRLF later.
            self.reset();
            let mut lines=Vec::new();let mut state=None;
            let mut parts=code.split('\n').peekable();
            while let Some(line)=parts.next() {
                let line=if parts.peek().is_some() { line.strip_suffix('\r').unwrap_or(line) } else { line };
                let line=Rc::new(self.tokenizer.line(line,state.as_ref())?);
                self.parsed_lines+=1;state=Some(line.state.clone());lines.push(line);
            }
            return Ok(lines);
        }
        if !code.starts_with(&self.prefix) { self.reset(); }
        let end=code.rfind('\n').map_or(0,|offset| offset+1);
        if end > self.prefix.len() {
            for line in code[self.prefix.len()..end-1].split('\n') {
                let line=Rc::new(self.tokenizer.line(line,self.state.as_ref())?);
                self.parsed_lines+=1;self.state=Some(line.state.clone());self.completed.push(line);
            }
            self.prefix.push_str(&code[self.prefix.len()..end]);
        }
        let mut lines=self.completed.clone();
        lines.push(Rc::new(self.tokenizer.line(&code[self.prefix.len()..],self.state.as_ref())?));
        self.parsed_lines+=1;
        Ok(lines)
    }
    fn reset(&mut self) { self.prefix.clear();self.completed.clear();self.state=None; }
}
