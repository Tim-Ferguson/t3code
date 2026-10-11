//! Shared Rust highlighting service. The only browser engine dependency is the
//! pinned Oniguruma scanner WASM; native targets call the same-version C engine.
use dioxus::prelude::*;
use futures_util::{future::{LocalBoxFuture,Shared},FutureExt};
use std::{cell::RefCell,rc::Rc};
use t3_highlight::{catalog::Catalog,scanner::{Error,Input},streaming::Streaming,theme::{Style,Theme},tokenizer::{Line,Tokenizer}};
#[cfg(target_arch="wasm32")]
type Engine=t3_highlight::browser::BrowserEngine;
#[cfg(not(target_arch="wasm32"))]
type Engine=t3_highlight::native::NativeEngine;
struct Service {catalog:RefCell<Catalog>,engine:Engine,themes:[Theme;2]}
type Initialization=Shared<LocalBoxFuture<'static,Result<Rc<Service>,Error>>>;
thread_local! {static INITIALIZATION:RefCell<Option<Initialization>>=const{RefCell::new(None)};}
async fn service()->Result<Rc<Service>,Error> {
    let initialization=INITIALIZATION.with(|slot|slot.borrow_mut().get_or_insert_with(||async {
        #[cfg(target_arch="wasm32")]
        let engine=Engine::load().await?;
        #[cfg(not(target_arch="wasm32"))]
        let engine=Engine::default();
        let themes=t3_highlight::catalog::themes()?.try_into().map_err(|_|Error("missing syntax themes".into()))?;
        Ok(Rc::new(Service{catalog:RefCell::new(Catalog::new()?),engine,themes}))
    }.boxed_local().shared()).clone());
    let result=initialization.clone().await;
    if result.is_err() {
        // A late failed waiter must not remove a replacement initialization.
        INITIALIZATION.with(|slot|{
            let mut slot=slot.borrow_mut();
            if slot.as_ref().is_some_and(|current|current.ptr_eq(&initialization)){slot.take();}
        });
    }
    result
}
struct Session {stream:Streaming<Engine>,service:Rc<Service>,rendered:Vec<(Rc<Line>,Rc<RenderedLine>)>}
async fn session(language:&str)->Result<Option<Rc<RefCell<Session>>>,Error> {
    let service=match service().await {
        Ok(service)=>service,
        // Source nontext initialization falls back to text, whose failed
        // initialization is removed from cache so a later request can retry.
        Err(error) if matches!(language,"text"|"plaintext"|"plain"|"txt"|"")=>return Err(error),
        Err(_)=>service().await?,
    };
    let grammar=service.catalog.borrow_mut().grammar(language)?;
    Ok(grammar.map(|grammar|Rc::new(RefCell::new(Session{stream:Streaming::new(Tokenizer::new(grammar,service.engine.clone())),service,rendered:Vec::new()}))))
}
#[derive(Clone,PartialEq,Eq)]
struct Span {text:String,style:String}
#[derive(PartialEq,Eq)]
pub(crate) struct RenderedLine {spans:Vec<Span>}
impl Session {
    fn document(&mut self,code:&str)->Result<Vec<Rc<RenderedLine>>,Error> {
        let tokenized=self.stream.document(code)?;
        let mut raw=code.split_inclusive('\n');
        let mut next=Vec::with_capacity(tokenized.len());
        for (index,line) in tokenized.into_iter().enumerate() {
            let text=raw.next().unwrap_or("");
            if let Some((prior,rendered))=self.rendered.get(index).filter(|(prior,_)|Rc::ptr_eq(prior,&line)) {
                next.push((prior.clone(),rendered.clone()));continue;
            }
            let body=text.strip_suffix('\n').unwrap_or(text);
            let normalized=if text.ends_with('\n'){body.strip_suffix('\r').unwrap_or(body)}else{body};
            let input=Input::from_str(normalized);let mut spans=Vec::new();
            for token in &line.tokens {
                let start=input.byte_offset(token.range.start.min(input.len())).min(normalized.len());
                let end=input.byte_offset(token.range.end.min(input.len())).min(normalized.len());
                if start==end {continue}
                let light=self.service.themes[0].style(&token.scopes);let dark=self.service.themes[1].style(&token.scopes);
                spans.push(Span{text:normalized[start..end].into(),style:dual_style(&light,&dark)});
            }
            // Retain exact input bytes, including CRLF and final newline.
            let suffix=&text[normalized.len()..];
            if !suffix.is_empty(){spans.push(Span{text:suffix.into(),style:String::new()});}
            next.push((line,Rc::new(RenderedLine{spans})));
        }
        self.rendered=next;
        Ok(self.rendered.iter().map(|(_,line)|line.clone()).collect())
    }
}
fn dual_style(light:&Style,dark:&Style)->String {
    fn mode(style:&Style,name:&str)->String {
        let weight=if style.font&2!=0{"bold"}else{"normal"};let italic=if style.font&1!=0{"italic"}else{"normal"};
        let decoration=match (style.font&4!=0,style.font&8!=0){(true,true)=>"underline line-through",(true,false)=>"underline",(false,true)=>"line-through",_=>"none"};
        format!("--syntax-{name}-fg:{};--syntax-{name}-weight:{weight};--syntax-{name}-style:{italic};--syntax-{name}-decoration:{decoration};",style.foreground.as_deref().unwrap_or("inherit"))
    }
    mode(light,"light")+&mode(dark,"dark")
}
#[derive(Props,Clone)]
pub(crate) struct LineProps {line:Rc<RenderedLine>}
impl PartialEq for LineProps {fn eq(&self,other:&Self)->bool{Rc::ptr_eq(&self.line,&other.line)}}
pub(crate) fn highlighted_line(props:LineProps)->Element {
    rsx!{span {class:"syntax-line",for (index,span) in props.line.spans.iter().enumerate(){span {key:"{index}",class:"syntax-token",style:span.style.clone(),"{span.text}"}}}}
}
#[component]
pub fn HighlightedCode(code:String,language:String)->Element {
    let loaded=use_resource(use_reactive((&language,),|(language,)|async move{
        let result=session(&language).await;
        (language,result)
    }));
    let lines=use_memo(use_reactive((&code,&language),move|(code,language)|{
        let loaded=loaded.read();
        match loaded.as_ref(){
            Some((owner,Ok(Some(session)))) if owner==&language=>session.borrow_mut().document(&code).map(Some),
            Some((owner,Err(error))) if owner==&language=>Err(error.clone()),
            _=>Ok(None),
        }
    }));
    match &*lines.read() {
        Ok(Some(lines))=>rsx!{code {"data-highlighted":"rust-textmate",for (index,line) in lines.iter().enumerate(){highlighted_line {key:"{index}",line:line.clone()}}}},
        Err(error)=>rsx!{code {"data-highlight-error":error.to_string(),"{code}"}},
        _=>rsx!{code {"{code}"}},
    }
}

/// Highlighted lines of a document, for callers that draw their own rows (the diff view).
/// `None` until the grammar is loaded and for languages without one; the caller draws plain
/// text meanwhile. Re-tokenizing after the document grows reuses every unchanged line.
pub(crate) fn use_lines(code:&str,language:&str)->Memo<Option<Vec<Rc<RenderedLine>>>> {
    let language=language.to_owned();let code=code.to_owned();
    let loaded=use_resource(use_reactive((&language,),|(language,)|async move{
        let result=session(&language).await;
        (language,result)
    }));
    use_memo(use_reactive((&code,&language),move|(code,language)|{
        match &*loaded.read(){
            Some((owner,Ok(Some(session)))) if owner==&language && !code.is_empty()=>session.borrow_mut().document(&code).ok(),
            _=>None,
        }
    }))
}

/// One run of a highlighted line, cut where a mark starts or ends.
pub(crate) struct Piece {
    pub text: String,
    pub style: String,
    pub marked: bool,
}
/// A line cut into runs at the boundaries of `ranges` (character positions, sorted, not overlapping),
/// each keeping its colours. Without a highlighted line the text is one plain run per cut.
pub(crate) fn split_marked(line: Option<&Rc<RenderedLine>>, text: &str, ranges: &[(usize, usize)]) -> Vec<Piece> {
    let spans: Vec<(String, String)> = match line {
        Some(line) if line.spans.iter().map(|span| span.text.chars().count()).sum::<usize>() >= text.chars().count() => line.spans.iter().map(|span| (span.text.clone(), span.style.clone())).collect(),
        _ => vec![(text.to_owned(), String::new())],
    };
    let mut pieces: Vec<Piece> = vec![];
    let mut position = 0usize;
    for (span_text, style) in spans {
        for character in span_text.chars() {
            let marked = ranges.iter().any(|(start, end)| (*start..*end).contains(&position));
            position += 1;
            match pieces.last_mut() {
                Some(last) if last.marked == marked && last.style == style => last.text.push(character),
                _ => pieces.push(Piece { text: character.to_string(), style: style.clone(), marked }),
            }
        }
    }
    pieces
}
