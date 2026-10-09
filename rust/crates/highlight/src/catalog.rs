//! Pinned data is loaded per requested language; no tokenizer JavaScript or
//! eager parse of all 253 grammars is needed at application startup.
use crate::{grammar::{Grammar,Registry},scanner::Error};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::{collections::{BTreeMap,BTreeSet},io::Read,rc::Rc};

pub struct Catalog {
    index: Value,
    registry: Registry,
    loaded: BTreeSet<String>,
    compiled: BTreeMap<String,Rc<Grammar>>,
}
impl Catalog {
    pub fn new()->Result<Self,Error> {
        let index=serde_json::from_str(include_str!("../assets/catalog.json")).map_err(|error|Error(error.to_string()))?;
        Ok(Self {index,registry:Registry::default(),loaded:BTreeSet::new(),compiled:BTreeMap::new()})
    }
    pub fn canonical_scope(&self,language:&str)->Option<&str> {
        self.index["languages"][language]["scope"].as_str()
    }
    /// Unknown languages use plain text, matching the source highlighter's
    /// fallback. Failed compilation is never stored as a successful cache hit.
    pub fn grammar(&mut self,language:&str)->Result<Option<Rc<Grammar>>,Error> {
        let Some(scope)=self.canonical_scope(language).map(str::to_owned) else{return Ok(None)};
        if let Some(grammar)=self.compiled.get(&scope) {return Ok(Some(grammar.clone()))}
        let bundled=self.index["languages"][language]["grammars"].as_array().ok_or_else(||Error("invalid language catalog".into()))?
            .iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>();
        for dependency in bundled {self.load(&dependency)?;}
        let grammar=Rc::new(self.registry.compile(&scope)?);
        self.compiled.insert(scope,grammar.clone());
        Ok(Some(grammar))
    }
    fn load(&mut self,scope:&str)->Result<(),Error> {
        if self.loaded.contains(scope){return Ok(())}
        let row=&self.index["scopes"][scope];
        let number=|key:&str|row[key].as_u64().and_then(|value|usize::try_from(value).ok()).ok_or_else(||Error(format!("missing grammar {scope}")));
        let start=number("offset")?;let length=number("length")?;let expanded=number("bytes")?;
        let bytes=include_bytes!("../assets/grammars.pack");
        let end=start.checked_add(length).ok_or_else(||Error("invalid grammar offset".into()))?;
        let compressed=bytes.get(start..end).ok_or_else(||Error("invalid grammar range".into()))?;
        let mut decoder=GzDecoder::new(compressed).take(expanded as u64+1);
        let mut data=Vec::with_capacity(expanded);
        decoder.read_to_end(&mut data).map_err(|error|Error(format!("{scope}: {error}")))?;
        if data.len()!=expanded {return Err(Error(format!("invalid grammar length {scope}")))}
        let grammar:Value=serde_json::from_slice(&data).map_err(|error|Error(format!("{scope}: {error}")))?;
        if grammar["scopeName"]!=scope {return Err(Error(format!("invalid grammar identity {scope}")))}
        // Break include cycles before walking dependencies. On failure remove
        // this admission so a later load can retry rather than cache the error.
        self.loaded.insert(scope.into());
        let result=(||{
            let mut dependencies=BTreeSet::new();collect_includes(&grammar,&mut dependencies);
            for dependency in dependencies {
                if self.index["scopes"].get(&dependency).is_some(){self.load(&dependency)?;}
            }
            self.registry.add(grammar)
        })();
        if result.is_err(){self.loaded.remove(scope);}
        result
    }
}
fn collect_includes(value:&Value,output:&mut BTreeSet<String>) {
    match value {
        Value::Array(items)=>for item in items {collect_includes(item,output)},
        Value::Object(fields)=>{
            if let Some(include)=fields.get("include").and_then(Value::as_str) {
                if !include.starts_with(['#','$']) {output.insert(include.split('#').next().unwrap_or(include).into());}
            }
            for value in fields.values(){collect_includes(value,output)}
        },
        _=>{},
    }
}
pub fn themes()->Result<Vec<crate::theme::Theme>,Error> {
    let data:Vec<Value>=serde_json::from_str(include_str!("../assets/themes.json")).map_err(|error|Error(error.to_string()))?;
    Ok(data.iter().map(crate::theme::Theme::new).collect())
}
