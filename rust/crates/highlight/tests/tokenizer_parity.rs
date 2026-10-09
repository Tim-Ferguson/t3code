use serde_json::{Value, json};
use t3_highlight::{grammar::Registry, native::NativeEngine};
use t3_highlight::{tokenizer, theme, streaming};

fn registry() -> Registry {
    let data: Vec<Value> = serde_json::from_str(include_str!("fixtures/grammars.json")).unwrap();
    let mut registry = Registry::default();
    for grammar in data { registry.add(grammar).unwrap(); }
    registry
}
fn expected_line(actual: &tokenizer::Line) -> Value {
    json!({ "tokens": actual.tokens.iter().map(|token| json!({ "startIndex": token.range.start,
        "endIndex": token.range.end, "scopes": token.scopes })).collect::<Vec<_>>(),
        "state": actual.state.inspect().map(|frame| json!({"names":frame.names,"content":frame.content,
            "end":frame.end,"beganAtEol":frame.began_at_eol})).collect::<Vec<_>>() })
}
#[test]
fn original_real_grammars_scope_ranges_and_multiline_state() {
    let registry = registry();
    let mut count = 0;
    for row in include_str!("fixtures/tokenizer.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let scope = row["scope"].as_str().unwrap();
        let mut tokenizer = tokenizer::Tokenizer::new(registry.compile(scope).unwrap(), NativeEngine);
        let mut state = None;
        for line in row["lines"].as_array().unwrap() {
            let actual = tokenizer.line(line["line"].as_str().unwrap(), state.as_ref()).unwrap();
            assert_eq!(expected_line(&actual), json!({"tokens":line["tokens"],"state":line["state"]}), "{scope} {:?}",line["line"]);
            state = Some(actual.state);count+=1;
        }
    }
    assert_eq!(count,54);
}
#[test]
fn original_pierre_colors_font_styles_and_scope_inheritance() {
    let registry = registry();
    let themes: Vec<Value> = serde_json::from_str(include_str!("fixtures/themes.json")).unwrap();
    for row in include_str!("fixtures/colored.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let theme = theme::Theme::new(themes.iter().find(|theme| theme["name"] == row["theme"]).unwrap());
        let mut tokenizer = tokenizer::Tokenizer::new(registry.compile(row["scope"].as_str().unwrap()).unwrap(), NativeEngine);
        let mut state = None;
        for line in row["lines"].as_array().unwrap() {
            let actual = tokenizer.line(line["line"].as_str().unwrap(),state.as_ref()).unwrap();
            let mut tokens = Vec::new();let mut previous = None;
            for token in actual.tokens {
                let style = theme.style(&token.scopes);
                let kind = theme::token_type(&token.scopes);
                if previous.as_ref() != Some(&(style.clone(),kind)) { tokens.push(json!({"start":token.range.start,"fg":style.foreground,"bg":style.background,"font":style.font,"type":kind})); }
                previous=Some((style,kind));
            }
            assert_eq!(json!(tokens),line["tokens"],"{} {} {:?}",row["scope"],row["theme"],line["line"]);
            state=Some(actual.state);
        }
    }
}

#[test]
fn streamed_suffix_equals_original_full_parse_without_reparsing_completed_lines() {
    let registry=registry();let mut count=0;
    for row in include_str!("fixtures/streaming.jsonl").lines() {
        let row:Value=serde_json::from_str(row).unwrap();
        let mut stream=streaming::Streaming::new(tokenizer::Tokenizer::new(registry.compile(row["scope"].as_str().unwrap()).unwrap(),NativeEngine));
        let mut completed = Vec::new();
        for prefix in row["prefixes"].as_array().unwrap() {
            let before=stream.parsed_lines;
            let actual=stream.document(prefix["prefix"].as_str().unwrap()).unwrap();
            assert_eq!(actual.len(),prefix["lines"].as_array().unwrap().len());
            for (index,line) in actual.iter().enumerate() {
                let expected=&prefix["lines"][index];
                assert_eq!(expected_line(line),json!({"tokens":expected["tokens"],"state":expected["state"]}),"{} prefix {:?} line{index}",row["scope"],prefix["prefix"]);
            }
            for (prior,current) in completed.iter().zip(&actual) { assert!(std::rc::Rc::ptr_eq(prior,current)); }
            assert!(stream.parsed_lines-before <= actual.len()-completed.len()+1);
            completed=actual[..actual.len()-1].to_vec();count+=1;
        }
    }
    assert_eq!(count,178);
}

#[test]
fn original_edits_shrinks_crlf_split_and_lone_cr_transitions() {
    let registry=registry();let mut count=0;
    for row in include_str!("fixtures/transitions.jsonl").lines() {
        let row:Value=serde_json::from_str(row).unwrap();
        let mut stream=streaming::Streaming::new(tokenizer::Tokenizer::new(registry.compile(row["scope"].as_str().unwrap()).unwrap(),NativeEngine));
        for version in row["versions"].as_array().unwrap() {
            let actual=stream.document(version["text"].as_str().unwrap()).unwrap();
            assert_eq!(actual.len(),version["lines"].as_array().unwrap().len());
            for (line,expected) in actual.iter().zip(version["lines"].as_array().unwrap()) {
                assert_eq!(expected_line(line),json!({"tokens":expected["tokens"],"state":expected["state"]}),"{} {:?}",row["name"],version["text"]);
            }
            count+=1;
        }
    }
    assert_eq!(count,17);
}

#[test]
fn lazy_catalog_aliases_share_grammar_unknown_languages_fall_back_and_source_smoke_matches() {
    let mut catalog=t3_highlight::catalog::Catalog::new().unwrap();
    let rust=catalog.grammar("rust").unwrap().unwrap();
    let alias=catalog.grammar("rs").unwrap().unwrap();
    assert!(std::rc::Rc::ptr_eq(&rust,&alias));
    assert!(catalog.grammar("not-a-supported-language").unwrap().is_none());
    assert!(catalog.grammar("text").unwrap().is_none());
    let mut count=0;
    for row in include_str!("fixtures/catalog.jsonl").lines() {
        let row:Value=serde_json::from_str(row).unwrap();
        let name=row["language"].as_str().unwrap();
        let grammar=catalog.grammar(name).unwrap_or_else(|error|panic!("{name}: {error}" )).unwrap();
        let mut tokenizer=tokenizer::Tokenizer::new(grammar,NativeEngine);let mut state=None;
        for expected in row["lines"].as_array().unwrap() {
            let line=tokenizer.line(expected["line"].as_str().unwrap(),state.as_ref()).unwrap_or_else(|error|panic!("{name}: {error}"));
            assert_eq!(expected_line(&line),json!({"tokens":expected["tokens"],"state":expected["state"]}),"catalog {name} {:?}",expected["line"]);
            state=Some(line.state);count+=1;
        }
    }
    assert_eq!(count,506);
}
