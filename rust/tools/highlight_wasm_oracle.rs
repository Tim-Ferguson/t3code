//! Development-only target oracle. Never linked into the application.
use serde_json::{Value,json};
use t3_highlight::{browser::BrowserEngine, grammar::Registry, regex::Source,
    scanner::{Engine,FindOptions,Input,Scanner}, streaming::Streaming,
    tokenizer::{Line,Tokenizer}, theme::{self,Theme}};
use wasm_bindgen::prelude::*;
fn failure(error: impl std::fmt::Display) -> JsValue { JsValue::from_str(&error.to_string()) }
fn input(value: &Value) -> Result<Input,JsValue> {
    serde_json::from_value(value.clone()).map(Input::from_units).map_err(failure)
}
#[wasm_bindgen]
pub struct Oracle { engine: BrowserEngine, registry: Registry }
#[wasm_bindgen]
pub async fn load(grammars: &str) -> Result<Oracle,JsValue> {
    let mut registry=Registry::default();
    for grammar in serde_json::from_str::<Vec<Value>>(grammars).map_err(failure)? { registry.add(grammar).map_err(failure)?; }
    Ok(Oracle {engine:BrowserEngine::load().await.map_err(failure)?,registry})
}
#[wasm_bindgen]
impl Oracle {
    pub fn scanner(&mut self,row: &str) -> Result<String,JsValue> {
        let row:Value=serde_json::from_str(row).map_err(failure)?;
        let value=&row["input"];
        let result=match row["kind"].as_str().unwrap_or("") {
            "input" => { let text=input(&value["units"])?;
                json!({"bytes":text.bytes(),"toByte":(0..text.len()+2).map(|i|text.byte_offset(i)).collect::<Vec<_>>(),
                    "toUnit":(0..text.bytes().len()+2).map(|i|text.unit_offset(i)).collect::<Vec<_>>()}) },
            "regex" => {
                let mut source=Source::new(&input(&value["pattern"])?);
                if !value["replacement"].is_null() {source.set_source(input(&value["replacement"] )?.units().to_vec());}
                let captures=value["captures"].as_array().ok_or_else(||failure("missing captures"))?.iter()
                    .map(|v|v["start"].as_u64().unwrap() as usize..v["end"].as_u64().unwrap() as usize).collect::<Vec<_>>();
                let anchors=[false,true].into_iter().flat_map(|a|[false,true].into_iter().map(move|g|(a,g)))
                    .map(|(a,g)|source.anchors(a,g).units().to_vec()).collect::<Vec<_>>();
                json!({"source":source.units(),"hasAnchor":source.has_anchor,"hasBackreferences":source.has_backreferences,
                    "anchors":anchors,"backreferences":source.backreferences(&input(&value["line"] )?,&captures).units()})
            },
            "match"|"reject" => {
                let patterns=value["patterns"].as_array().ok_or_else(||failure("missing patterns"))?.iter().map(input).collect::<Result<Vec<_>,_>>()?;
                if row["kind"]=="reject" { return Ok(json!(self.engine.compile(&patterns).is_err()).to_string()); }
                let mut scanner=self.engine.compile(&patterns).map_err(failure)?;
                scanner.find(&input(&value["text"] )?,value["start"].as_u64().unwrap(),FindOptions(value["options"].as_u64().unwrap() as u32))
                    .map_err(failure)?.map(|matched|json!({"index":matched.pattern,"captureIndices":matched.captures.iter()
                        .map(|range|json!({"start":range.start,"end":range.end,"length":range.end-range.start})).collect::<Vec<_>>()})).unwrap_or(Value::Null)
            },
            _ => return Err(failure("unknown scanner witness")),
        };
        Ok(result.to_string())
    }
    pub fn tokenizer(&self,scope:&str,theme:&str) -> Result<StreamOracle,JsValue> {
        let tokenizer=Tokenizer::new(self.registry.compile(scope).map_err(failure)?,self.engine.clone());
        let theme=if theme.is_empty(){None}else{Some(Theme::new(&serde_json::from_str::<Value>(theme).map_err(failure)?))};
        Ok(StreamOracle {stream:Streaming::new(tokenizer),theme})
    }
}
#[wasm_bindgen]
pub struct StreamOracle { stream:Streaming<BrowserEngine>,theme:Option<Theme> }
#[wasm_bindgen]
impl StreamOracle {
    pub fn document(&mut self,text:&str)->Result<String,JsValue> {
        let lines=self.stream.document(text).map_err(failure)?;
        Ok(json!(lines.iter().map(|line|line_json(line,self.theme.as_ref())).collect::<Vec<_>>()).to_string())
    }
}
fn line_json(line:&Line,theme:Option<&Theme>)->Value {
    let tokens=if let Some(theme)=theme {
        let mut result=Vec::new();let mut previous=None;
        for token in &line.tokens {
            let style=theme.style(&token.scopes);let kind=theme::token_type(&token.scopes);
            if previous.as_ref()!=Some(&(style.clone(),kind)) {result.push(json!({"start":token.range.start,"fg":style.foreground,"bg":style.background,"font":style.font,"type":kind}));}
            previous=Some((style,kind));
        }
        result
    } else {line.tokens.iter().map(|token|json!({"startIndex":token.range.start,"endIndex":token.range.end,"scopes":token.scopes})).collect()};
    json!({"tokens":tokens,"state":line.state.inspect().map(|frame|json!({"names":frame.names,"content":frame.content,"end":frame.end,"beganAtEol":frame.began_at_eol})).collect::<Vec<_>>()})
}
