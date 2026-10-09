use crate::{grammar::{Capture, Grammar, Kind, RuleId}, regex::Source,
    scanner::{Engine, Error, FindOptions, Input, Scanner}, scope};
use std::{collections::{BTreeMap, BTreeSet}, ops::Range, rc::Rc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token { pub range: Range<usize>, pub scopes: Vec<String> }
#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    rule: RuleId,
    entered: Option<usize>,
    anchor: Option<usize>,
    began_at_eol: bool,
    names: Vec<String>,
    content: Vec<String>,
    end: Option<Vec<u16>>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State { frames: Vec<Frame> }
pub struct StateFrame<'a> {
    pub names: &'a [String],
    pub content: &'a [String],
    pub end: Option<&'a [u16]>,
    pub began_at_eol: bool,
}
impl State {
    pub fn inspect(&self) -> impl Iterator<Item = StateFrame<'_>> {
        self.frames.iter().map(|frame| StateFrame { names: &frame.names,
            content: &frame.content, end: frame.end.as_deref(), began_at_eol: frame.began_at_eol })
    }
}
pub struct Line { pub tokens: Vec<Token>, pub state: State }
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SearchKey { rule: RuleId, end: Option<Vec<u16>>, begin: bool, position: bool, condition: bool }
struct Compiled<S> { scanner: S, rules: Vec<Option<RuleId>> }
pub struct Tokenizer<E: Engine> {
    grammar: Rc<Grammar>,
    engine: E,
    searches: BTreeMap<SearchKey, Compiled<E::Scanner>>,
}
impl<E: Engine> Tokenizer<E> {
    pub fn new(grammar: impl Into<Rc<Grammar>>, engine: E) -> Self { Self { grammar:grammar.into(), engine, searches: BTreeMap::new() } }
    pub fn line(&mut self, text: &str, previous: Option<&State>) -> Result<Line, Error> {
        let input = Input::from_str(&format!("{text}\n"));
        let first = previous.is_none_or(|state| state.frames.is_empty());
        let mut state = previous.cloned().unwrap_or_default();
        if first {
            let names = vec![self.grammar.scope.clone()];
            state.frames.push(Frame { rule: self.grammar.root, entered: None, anchor: None,
                began_at_eol: false, content: names.clone(), names, end: None });
        } else {
            for frame in &mut state.frames { frame.entered = None; frame.anchor = None; }
        }
        let mut tokens = Vec::new();
        self.scan(&input, first, 0, &mut state, &mut tokens, true, 0)?;
        if tokens.last().is_some_and(|token| token.range.start == input.len()-1) { tokens.pop(); }
        if tokens.is_empty() { tokens.push(Token { range: 0..input.len(), scopes: state.frames.last().unwrap().content.clone() }); }
        Ok(Line { tokens, state })
    }
    fn collect(&self, rule: RuleId, output: &mut Vec<(Option<RuleId>, Source)>, visiting: &mut BTreeSet<RuleId>) {
        if !visiting.insert(rule) { return; }
        match &self.grammar.rules[rule].kind {
            Kind::Match { expression, .. } => output.push((Some(rule), expression.clone())),
            Kind::BeginEnd { begin, .. } | Kind::BeginWhile { begin, .. } => output.push((Some(rule), begin.clone())),
            Kind::Container { patterns, .. } => for &child in patterns { self.collect(child, output, visiting); },
        }
        visiting.remove(&rule);
    }
    fn find(&mut self, input: &Input, start: usize, frame: &Frame, first: bool, position: bool, condition: bool)
        -> Result<Option<(Option<RuleId>, Vec<Range<usize>>)>, Error> {
        let key = SearchKey { rule: frame.rule, end: frame.end.clone(), begin: first, position, condition };
        if !self.searches.contains_key(&key) {
            let kind = &self.grammar.rules[frame.rule].kind;
            let mut expressions = Vec::new();
            if condition {
                if let Kind::BeginWhile { condition, .. } = kind { expressions.push((None, condition.clone())); }
            } else {
                if let Kind::Match { expression, .. } = kind { expressions.push((Some(frame.rule),expression.clone())); }
                for &child in kind.patterns() { self.collect(child, &mut expressions, &mut BTreeSet::new()); }
                if let Kind::BeginEnd { end, end_last, .. } = kind {
                    if *end_last { expressions.push((None, end.clone())); } else { expressions.insert(0, (None, end.clone())); }
                }
            }
            for (id, expression) in &mut expressions {
                if id.is_none() {
                    if let Some(end) = &frame.end { expression.set_source(end.clone()); }
                }
            }
            let patterns = expressions.iter().map(|(_, expression)| expression.anchors(first, position)).collect::<Vec<_>>();
            let scanner = self.engine.compile(&patterns)?;
            // Dynamic capture endings must not grow a process-wide scanner cache.
            // Keep the common grammar scans cached, bound dynamic end variants.
            if self.searches.len() >= 512 { self.searches.retain(|key, _| key.end.is_none()); }
            self.searches.insert(key.clone(), Compiled { scanner, rules: expressions.iter().map(|(id,_)| *id).collect() });
        }
        let compiled = self.searches.get_mut(&key).unwrap();
        let Some(matched) = compiled.scanner.find(input, start as u64, FindOptions::NONE)? else { return Ok(None); };
        let rule = *compiled.rules.get(matched.pattern).ok_or_else(|| Error("scanner returned unknown rule".into()))?;
        Ok(Some((rule, matched.captures)))
    }
    fn find_with_injections(&mut self, input: &Input, start: usize, frame: &Frame, first: bool, position: bool)
        -> Result<Option<(Option<RuleId>, Vec<Range<usize>>)>,Error> {
        let mut best = self.find(input,start,frame,first,position,false)?;
        let eligible: Vec<_> = self.grammar.injections.iter().filter(|(selector,_)| selector.matches(&frame.content))
            .map(|(selector,rule)| (selector.priority,*rule)).collect();
        let mut injected: Option<(i8,Option<RuleId>,Vec<Range<usize>>)> = None;
        for (priority,rule) in eligible {
            let mut injection=frame.clone();injection.rule=rule;injection.end=None;
            if let Some((matched,captures)) = self.find(input,start,&injection,first,position,false)? {
                if injected.as_ref().is_none_or(|(_,_,best)| captures[0].start < best[0].start) {
                    let at_start = captures[0].start == start;
                    injected=Some((priority,matched,captures));
                    if at_start { break; }
                }
            }
        }
        if let Some((priority,matched,captures)) = injected {
            if best.as_ref().is_none_or(|(_,best)| captures[0].start < best[0].start
                || (priority == -1 && captures[0].start == best[0].start)) { best=Some((matched,captures)); }
        }
        Ok(best)
    }
    fn scan(&mut self, input: &Input, mut first: bool, mut offset: usize, state: &mut State,
        tokens: &mut Vec<Token>, check_while: bool, recursion: usize) -> Result<(), Error> {
        if recursion > 256 { return Err(Error("grammar capture nesting exceeded".into())); }
        let mut anchor = None;
        if check_while {
            anchor = state.frames.last().filter(|frame| frame.began_at_eol).map(|_| 0);
            for index in 0..state.frames.len() {
                let frame = state.frames[index].clone();
                let Kind::BeginWhile { while_captures, .. } = self.grammar.rules[frame.rule].kind.clone() else { continue; };
                let Some((None, captures)) = self.find(input, offset, &frame, first, anchor == Some(offset), true)? else {
                    state.frames.truncate(index); break;
                };
                emit(tokens, &frame.content, captures[0].start);
                self.captures(input, first, &frame, tokens, &while_captures, &captures, recursion)?;
                emit(tokens, &frame.content, captures[0].end);
                anchor = Some(captures[0].end);
                if captures[0].end > offset { offset = captures[0].end; first = false; }
            }
        }
        loop {
            let frame = state.frames.last().cloned().ok_or_else(|| Error("grammar stack lost root".into()))?;
            let Some((matched, captures)) = self.find_with_injections(input, offset, &frame, first, anchor == Some(offset))? else {
                emit(tokens, &frame.content, input.len()); break;
            };
            let range = captures.first().ok_or_else(|| Error("match has no whole capture".into()))?.clone();
            let advanced = range.end > offset;
            emit(tokens, &frame.content, range.start);
            if let Some(id) = matched {
                let rule = self.grammar.rules[id].clone();
                let names = scope::push(&frame.content, rule.name.as_deref(), input, &captures);
                let mut next = Frame { rule: id, entered: Some(offset), anchor, began_at_eol: range.end == input.len(),
                    names: names.clone(), content: names, end: None };
                let (begin_captures, ending) = match &rule.kind {
                    Kind::Match { captures, .. } => (captures, None),
                    Kind::BeginEnd { begin_captures, end, .. } => (begin_captures, Some(end)),
                    Kind::BeginWhile { begin_captures, condition, .. } => (begin_captures, Some(condition)),
                    _ => return Err(Error("include-only rule matched".into())),
                };
                self.captures(input, first, &next, tokens, begin_captures, &captures, recursion)?;
                emit(tokens, &next.content, range.end);
                if ending.is_some() {
                    anchor = Some(range.end);
                    next.content = scope::push(&next.names, rule.content_name.as_deref(), input, &captures);
                    if let Some(ending) = ending.filter(|ending| ending.has_backreferences) {
                        next.end = Some(ending.backreferences(input, &captures).units().to_vec());
                    }
                    let repeated = state.frames.iter().rev().take_while(|frame| frame.entered == Some(offset)).any(|frame| frame.rule == id);
                    if !advanced && repeated { emit(tokens, &frame.content, input.len()); break; }
                    state.frames.push(next);
                } else if !advanced {
                    if state.frames.len() > 1 { state.frames.pop(); }
                    emit(tokens, &state.frames.last().unwrap().content, input.len()); break;
                }
            } else {
                let Kind::BeginEnd { end_captures, .. } = self.grammar.rules[frame.rule].kind.clone() else { return Err(Error("non-end rule closed".into())); };
                let mut closing = frame.clone(); closing.content = closing.names.clone();
                self.captures(input, first, &closing, tokens, &end_captures, &captures, recursion)?;
                emit(tokens, &closing.content, range.end);
                if !advanced && frame.entered == Some(offset) {
                    state.frames.last_mut().unwrap().content = closing.content.clone();
                    emit(tokens, &closing.content, input.len()); break;
                }
                state.frames.pop(); anchor = frame.anchor;
            }
            if range.end > offset { offset = range.end; first = false; }
        }
        Ok(())
    }
    fn captures(&mut self, input: &Input, first: bool, frame: &Frame, tokens: &mut Vec<Token>,
        rules: &[Option<Capture>], ranges: &[Range<usize>], recursion: usize) -> Result<(), Error> {
        let mut local: Vec<(Vec<String>, usize)> = Vec::new();
        let max = ranges.first().map_or(0, |range| range.end);
        for (rule, range) in rules.iter().zip(ranges) {
            let Some(rule) = rule else { continue; };
            if range.is_empty() { continue; }
            if range.start > max { break; }
            while local.last().is_some_and(|(_,end)| *end <= range.start) {
                let (scopes, end) = local.pop().unwrap(); emit(tokens, &scopes, end);
            }
            let base = local.last().map_or(&frame.content, |(scopes,_)| scopes);
            emit(tokens, base, range.start);
            if let Some(id) = rule.retokenize {
                let names = scope::push(&frame.content, rule.name.as_deref(), input, ranges);
                let content = scope::push(&names, rule.content_name.as_deref(), input, ranges);
                let mut nested = State { frames: vec![Frame { rule: id, entered: Some(range.start), anchor: None,
                    began_at_eol: false, names, content, end: None }] };
                let prefix = Input::from_units(input.units()[..range.end.min(input.len())].to_vec());
                self.scan(&prefix, first && range.start == 0, range.start, &mut nested, tokens, false, recursion+1)?;
            } else if rule.name.is_some() {
                local.push((scope::push(base, rule.name.as_deref(), input, ranges), range.end));
            }
        }
        while let Some((scopes, end)) = local.pop() { emit(tokens, &scopes, end); }
        Ok(())
    }
}
fn emit(tokens: &mut Vec<Token>, scopes: &[String], end: usize) {
    let start = tokens.last().map_or(0, |token| token.range.end);
    if start < end { tokens.push(Token { range: start..end, scopes: scopes.to_vec() }); }
}
