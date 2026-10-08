//! Rust markdown nodes keep code controls mounted while a reply streams. Raw
//! HTML retains the existing escaped boundary until the source sanitizer lands.
use dioxus::prelude::*;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag};
use std::{ops::Range, rc::Rc};
use t3_client::markdown as policy;

#[derive(Clone, PartialEq)]
enum Kind {
    Paragraph,
    Heading(u8),
    Quote(Option<String>),
    List(Option<u64>),
    Item,
    Strong,
    Emphasis,
    Strike,
    Code,
    Break,
    Rule,
    Task(bool),
    Table(Vec<Alignment>),
    Head,
    Row,
    Cell(bool, Alignment),
    Link(String, String),
    Image(String, String),
    Fence(String),
    Transparent,
}
#[derive(Clone, PartialEq)]
struct Node {
    kind: Kind,
    text: String,
    children: Rc<Vec<Node>>,
    range: Range<usize>,
}
impl Node {
    fn new(kind: Kind, range: Range<usize>) -> Self {
        Self {
            kind,
            text: String::new(),
            children: Rc::new(Vec::new()),
            range,
        }
    }
    fn plain_text(&self) -> String {
        self.text.clone()
            + &self
                .children
                .iter()
                .map(Self::plain_text)
                .collect::<String>()
    }
}
fn parse(text: &str) -> Vec<Node> {
    let mut stack = vec![Node::new(Kind::Transparent, 0..text.len())];
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                let kind = match tag {
                    Tag::Paragraph => Kind::Paragraph,
                    Tag::Heading { level, .. } => Kind::Heading(level as u8),
                    Tag::BlockQuote(kind) => Kind::Quote(kind.map(|kind| format!("{kind:?}"))),
                    Tag::List(start) => Kind::List(start),
                    Tag::Item => Kind::Item,
                    Tag::Strong => Kind::Strong,
                    Tag::Emphasis => Kind::Emphasis,
                    Tag::Strikethrough => Kind::Strike,
                    Tag::Table(alignments) => Kind::Table(alignments),
                    Tag::TableHead => Kind::Head,
                    Tag::TableRow => Kind::Row,
                    Tag::TableCell => {
                        let column = stack.last().unwrap().children.len();
                        let alignment = stack
                            .iter()
                            .rev()
                            .find_map(|node| match &node.kind {
                                Kind::Table(alignments) => alignments.get(column).copied(),
                                _ => None,
                            })
                            .unwrap_or(Alignment::None);
                        Kind::Cell(stack.iter().any(|node| node.kind == Kind::Head), alignment)
                    }
                    Tag::Link {
                        dest_url, title, ..
                    } => Kind::Link(dest_url.into_string(), title.into_string()),
                    Tag::Image {
                        dest_url, title, ..
                    } => Kind::Image(dest_url.into_string(), title.into_string()),
                    Tag::CodeBlock(CodeBlockKind::Fenced(info)) => Kind::Fence(info.into_string()),
                    Tag::CodeBlock(CodeBlockKind::Indented) => Kind::Fence(String::new()),
                    _ => Kind::Transparent,
                };
                stack.push(Node::new(kind, range));
            }
            Event::End(_) => {
                let mut node = stack.pop().expect("balanced markdown parser");
                node.range.end = range.end;
                // remark-rehype appends a newline to every code node, including
                // an unfinished fence at EOF. cmark retains only authored LFs.
                if matches!(node.kind, Kind::Fence(_)) && !node.plain_text().ends_with('\n') {
                    let mut trailing = Node::new(Kind::Transparent, node.range.end..node.range.end);
                    trailing.text = "\n".into();
                    Rc::make_mut(&mut node.children).push(trailing);
                }
                Rc::make_mut(&mut stack.last_mut().unwrap().children).push(node);
            }
            event => {
                let mut node = match event {
                    Event::Text(_) | Event::Html(_) | Event::InlineHtml(_) => {
                        Node::new(Kind::Transparent, range)
                    }
                    Event::Code(_) => Node::new(Kind::Code, range),
                    Event::SoftBreak => Node::new(Kind::Transparent, range),
                    Event::HardBreak => Node::new(Kind::Break, range),
                    Event::Rule => Node::new(Kind::Rule, range),
                    Event::TaskListMarker(checked) => Node::new(Kind::Task(checked), range),
                    _ => Node::new(Kind::Transparent, range),
                };
                node.text = match event {
                    Event::Text(value)
                    | Event::Html(value)
                    | Event::InlineHtml(value)
                    | Event::Code(value) => value.into_string(),
                    Event::SoftBreak => "\n".into(),
                    _ => String::new(),
                };
                Rc::make_mut(&mut stack.last_mut().unwrap().children).push(node);
            }
        }
    }
    Rc::try_unwrap(stack.pop().unwrap().children).unwrap_or_else(|nodes| (*nodes).clone())
}

#[derive(Default)]
struct Cache {
    prefix: String,
    nodes: Vec<Node>,
}
impl Cache {
    fn parse(&mut self, text: &str) -> Vec<Node> {
        // Match the source's conservative parser boundary: completed top-level
        // fences followed by a blank line. Definitions may alter earlier links;
        // CRLF/BOM boundaries must be parsed in the full document context.
        let can_reuse = !text.contains(['\r', '\u{FEFF}']) && text.starts_with(&self.prefix);
        let offset = if can_reuse { self.prefix.len() } else { 0 };
        let suffix = &text[offset..];
        let parser = Parser::new_ext(suffix, Options::ENABLE_FOOTNOTES);
        let definitions = parser.reference_definitions().iter().next().is_some()
            || parser
                .into_iter()
                .any(|event| matches!(event, Event::Start(Tag::FootnoteDefinition(_))));
        let nodes = if definitions || !can_reuse {
            parse(text)
        } else {
            let mut nodes = self.nodes.clone();
            let mut tail = parse(suffix);
            fn shift(node: &mut Node, offset: usize) {
                node.range.start += offset;
                node.range.end += offset;
                for child in Rc::make_mut(&mut node.children) {
                    shift(child, offset);
                }
            }
            if offset > 0 {
                for node in &mut tail {
                    shift(node, offset);
                }
            }
            nodes.extend(tail);
            nodes
        };
        if definitions || text.contains(['\r', '\u{FEFF}']) {
            self.prefix.clear();
            self.nodes.clear();
            return nodes;
        }
        for index in (0..nodes.len()).rev() {
            let node = &nodes[index];
            if !matches!(node.kind, Kind::Fence(_)) {
                continue;
            }
            let Some(source) = text.get(node.range.clone()) else {
                continue;
            };
            // cmark includes the closing line terminator in its range, while
            // mdast stops at the delimiter. Retain exactly one separator LF.
            let end = node.range.end - usize::from(source.ends_with('\n'));
            let source = &text[node.range.start..end];
            let source = source.trim_start_matches(' ');
            if text[node.range.start..end].len() - source.len() > 3 || !policy::closed_fence(source)
            {
                continue;
            }
            let rest = &text[end..];
            let Some(after_line) = rest.strip_prefix('\n') else {
                continue;
            };
            let blank = after_line.trim_start_matches([' ', '\t']);
            if !blank.starts_with('\n') {
                continue;
            }
            let boundary = end + 1 + (after_line.len() - blank.len()) + 1;
            self.prefix = text[..boundary].to_owned();
            self.nodes = nodes[..=index].to_vec();
            break;
        }
        nodes
    }
}

#[component]
pub(crate) fn Markdown(text: String) -> Element {
    let cache = use_hook(|| Rc::new(std::cell::RefCell::new(Cache::default())));
    let nodes = use_memo(use_reactive((&text,), move |(text,)| {
        Rc::new(cache.borrow_mut().parse(&text))
    }));
    rsx! {div {class:"markdown",for node in nodes.read().iter(){RenderNode{key:"{node.range.start}:{kind_key(&node.kind)}",node:node.clone()}}}}
}
fn kind_key(kind: &Kind) -> &'static str {
    match kind {
        Kind::Fence(_) => "fence",
        Kind::Table(_) => "table",
        _ => "node",
    }
}
#[component]
fn Children(nodes: Rc<Vec<Node>>) -> Element {
    rsx! {for node in nodes.iter(){RenderNode{key:"{node.range.start}:{kind_key(&node.kind)}",node:node.clone()}}}
}
#[component]
fn RenderNode(node: Node) -> Element {
    let children = rsx! {Children {nodes:node.children.clone()}};
    match &node.kind {
        Kind::Paragraph => rsx! {p {"{node.text}" {children}}},
        Kind::Heading(1) => rsx! {h1 {{children}}},
        Kind::Heading(2) => rsx! {h2 {{children}}},
        Kind::Heading(3) => rsx! {h3 {{children}}},
        Kind::Heading(4) => rsx! {h4 {{children}}},
        Kind::Heading(5) => rsx! {h5 {{children}}},
        Kind::Heading(_) => rsx! {h6 {{children}}},
        Kind::Quote(label) => {
            rsx! {blockquote {class:if label.is_some(){"markdown-alert"}else{""},if let Some(label)=label {strong {"{label}"}}{children}}}
        }
        Kind::List(Some(start)) => rsx! {ol {start:*start as i64,{children}}},
        Kind::List(None) => rsx! {ul {{children}}},
        Kind::Item => rsx! {li {{children}}},
        Kind::Strong => rsx! {strong {{children}}},
        Kind::Emphasis => rsx! {em {{children}}},
        Kind::Strike => rsx! {del {{children}}},
        Kind::Code => rsx! {code {"{node.text}"}},
        Kind::Break => rsx! {br {}},
        Kind::Rule => rsx! {hr {}},
        Kind::Task(checked) => {
            rsx! {input {r#type:"checkbox",checked:*checked,disabled:true,"aria-label":if *checked{"Completed task"}else{"Unchecked task"}}}
        }
        Kind::Table(_) => rsx! {div {class:"markdown-table",table {{children}}}},
        Kind::Head => rsx! {thead {tr {{children}}}},
        Kind::Row => rsx! {tr {{children}}},
        Kind::Cell(true, alignment) => rsx! {th {style:alignment_style(*alignment),{children}}},
        Kind::Cell(false, alignment) => rsx! {td {style:alignment_style(*alignment),{children}}},
        Kind::Link(href, title) if crate::safe_url(href) => {
            rsx! {a {href:href.clone(),title:title.clone(),{children}}}
        }
        Kind::Image(src, title) if crate::safe_url(src) => {
            rsx! {img {src:src.clone(),alt:node.plain_text(),title:title.clone()}}
        }
        Kind::Link(_, _) | Kind::Image(_, _) => rsx! {{children}},
        Kind::Fence(info) => {
            let (language, title) = fence_parts(info);
            rsx! {CodeBlock {code:node.plain_text(),language,title}}
        }
        Kind::Transparent => rsx! {"{node.text}" {children}},
    }
}
fn alignment_style(alignment: Alignment) -> &'static str {
    match alignment {
        Alignment::Left => "text-align:left;",
        Alignment::Center => "text-align:center;",
        Alignment::Right => "text-align:right;",
        Alignment::None => "",
    }
}
fn fence_parts(info: &str) -> (String, Option<String>) {
    // CommonMark info separates language/meta with ASCII spaces/tabs. The
    // renderer then applies its JavaScript regex/trim policy to those values.
    let (language, meta) = info
        .split_once([' ', '\t'])
        .map_or((info, None), |(lang, meta)| {
            (lang, Some(t3_client::provider_auth::trim(meta)))
        });
    let class = (!language.is_empty()).then(|| format!("language-{language}"));
    (
        policy::fence_language(class.as_deref()),
        policy::fence_title(meta),
    )
}

#[component]
fn CodeBlock(code: String, language: String, title: Option<String>) -> Element {
    let initial = try_consume_context::<crate::client_settings::Writer>()
        .map(|writer| writer.document.peek().snapshot().word_wrap)
        .unwrap_or(true);
    let mut wrapped = use_signal(|| initial);
    let mut copied = use_signal(|| false);
    let mut copy_error = use_signal(|| None::<String>);
    let mut revision = use_signal(|| 0u64);
    let copy_code = code.clone();
    let copy = EventHandler::new(move |_| {
        let bytes = serde_json::to_string(&copy_code).expect("clipboard text");
        // Start clipboard access in the click gesture; Rust owns its receipt and
        // local control lifetime. A later copy replaces the label-reset timer.
        let eval = document::eval(&format!(
            "await navigator.clipboard.writeText({bytes});return true;"
        ));
        revision += 1;
        let receipt = *revision.peek();
        spawn(async move {
            let result = eval.join::<bool>().await;
            if *revision.peek() != receipt {
                return;
            }
            match result {
                Ok(true) => {
                    copy_error.set(None);
                    copied.set(true);
                    delay(1200).await;
                    if *revision.peek() == receipt {
                        copied.set(false);
                    }
                }
                _ => copy_error.set(Some("Could not copy the code block.".into())),
            }
        });
    });
    rsx! {div {class:"chat-markdown-codeblock","data-language":language.clone(),"data-wrap":wrapped().to_string(),
        header {class:"chat-markdown-codeblock-header",span {class:"codeblock-title",title:language.clone(),{title.unwrap_or_else(||language.clone())}}
            span {role:"toolbar","aria-label":"Code block actions",
                button {r#type:"button","aria-label":if wrapped(){"Disable line wrap"}else{"Wrap lines"},"aria-pressed":wrapped(),onclick:move |_|wrapped.toggle(),"↵"}
                button {r#type:"button","aria-label":if copied(){"Copied"}else{"Copy code"},onclick:copy,if copied(){"✓"}else{"Copy"}}
            }
        }
        pre {style:if wrapped(){"white-space:pre-wrap;overflow-wrap:anywhere;"}else{"white-space:pre;overflow-wrap:normal;"},code {"{code}"}}
        if let Some(error)=copy_error(){p {role:"alert","{error}"}}
    }}
}

async fn delay(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn codes(nodes: &[Node], out: &mut Vec<Value>) {
        for node in nodes {
            if let Kind::Fence(info) = &node.kind {
                let (language, title) = fence_parts(info);
                out.push(json!({"language":language,"title":title,"code":node.plain_text()}));
            }
            codes(&node.children, out);
        }
    }
    #[test]
    fn original_parsed_fence_info_and_nested_source_documents() {
        for (index, line) in include_str!("../tests/fixtures/markdown-documents.jsonl")
            .lines()
            .enumerate()
        {
            let row: Value = serde_json::from_str(line).unwrap();
            let nodes = parse(row["source"].as_str().unwrap());
            if let Some(expected) = row.get("expected") {
                let mut actual = Vec::new();
                codes(&nodes, &mut actual);
                assert_eq!(
                    json!(actual),
                    *expected,
                    "original parsed document {index}: {}",
                    row["source"]
                );
            } else {
                let align = nodes
                    .iter()
                    .find_map(|node| match &node.kind {
                        Kind::Table(align) => Some(
                            align
                                .iter()
                                .map(|value| match value {
                                    Alignment::Left => json!("left"),
                                    Alignment::Center => json!("center"),
                                    Alignment::Right => json!("right"),
                                    Alignment::None => Value::Null,
                                })
                                .collect::<Vec<_>>(),
                        ),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(json!(align), row["align"]);
                let table = &nodes[0];
                for container in table.children.iter() {
                    for (column, cell) in container.children.iter().enumerate() {
                        if let Kind::Cell(_, alignment) = cell.kind {
                            assert_eq!(
                                alignment_style(alignment),
                                match row["align"][column].as_str() {
                                    Some("left") => "text-align:left;",
                                    Some("center") => "text-align:center;",
                                    Some("right") => "text-align:right;",
                                    _ => "",
                                }
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn completed_large_prefix_is_reused_and_document_definitions_invalidate_it() {
        let prefix = format!("```rust\n{}\n```\n\n", "let a = 42;\n".repeat(20000));
        let mut cache = Cache::default();
        let initial = cache.parse(&(prefix.clone() + "reply"));
        assert_eq!(cache.prefix, prefix);
        for suffix in ["reply a", "reply b\n\n```text\nopen", "reply c"] {
            let streamed = cache.parse(&(prefix.clone() + suffix));
            assert!(
                Rc::ptr_eq(&initial[0].children, &streamed[0].children),
                "completed code remains shared"
            );
            assert!(
                streamed == parse(&(prefix.clone() + suffix)),
                "incremental result equals full parse"
            );
        }
        let referenced = prefix + "[late][target]\n\n[target]: https://example.test\n";
        assert!(cache.parse(&referenced) == parse(&referenced));
        assert!(
            cache.prefix.is_empty(),
            "definitions require document-wide parsing"
        );
        for source in [
            "```\n\n",
            "```\n\nstreamed code",
            "```\n\nstreamed code\n```\n\nafter",
            "[target]: https://example.test\n\n```rust\nclosed\n```\n\n[link][target]",
            "```rust\nclosed\n```\n\n[link][target]\n\n[target]: https://example.test",
            "> ```rust\n> nested\n> ```\n\nafter",
            "- ```rust\n  nested\n  ```\n\nafter",
            "```rust\nclosed\n```\r\n\nafter",
            "```rust\nclosed\n```\n\n\u{FEFF}after",
        ] {
            assert!(
                cache.parse(source) == parse(source),
                "cached parse differs for {source:?}"
            );
        }
    }

    #[derive(Default)]
    struct Clipboard {
        writes: Vec<String>,
        allowed: bool,
        wakers: std::collections::BTreeMap<usize, std::task::Waker>,
        results: std::collections::BTreeMap<usize, bool>,
        observed: std::collections::BTreeSet<usize>,
    }
    impl Clipboard {
        fn complete(&mut self, request: usize, success: bool) {
            self.results.insert(request, success);
            if let Some(waker) = self.wakers.remove(&request) {
                waker.wake();
            }
        }
    }
    struct CopyEval(Rc<std::cell::RefCell<Clipboard>>, usize);
    impl document::Evaluator for CopyEval {
        fn send(&self, _: Value) -> Result<(), document::EvalError> {
            Ok(())
        }
        fn poll_recv(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<Value, document::EvalError>> {
            std::task::Poll::Pending
        }
        fn poll_join(
            &mut self,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<Value, document::EvalError>> {
            let mut clipboard = self.0.borrow_mut();
            if let Some(result) = clipboard
                .results
                .get(&self.1)
                .copied()
                .or(clipboard.allowed.then_some(true))
            {
                clipboard.observed.insert(self.1);
                std::task::Poll::Ready(Ok(json!(result)))
            } else {
                clipboard.wakers.insert(self.1, cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }
    struct CopyDocument {
        owner: dioxus::signals::Owner,
        clipboard: Rc<std::cell::RefCell<Clipboard>>,
    }
    impl document::Document for CopyDocument {
        fn eval(&self, script: String) -> document::Eval {
            let text = script
                .strip_prefix("await navigator.clipboard.writeText(")
                .unwrap()
                .strip_suffix(");return true;")
                .unwrap();
            let request = self.clipboard.borrow().writes.len();
            self.clipboard
                .borrow_mut()
                .writes
                .push(serde_json::from_str(text).unwrap());
            document::Eval::new(
                self.owner
                    .insert(Box::new(CopyEval(self.clipboard.clone(), request))
                        as Box<dyn document::Evaluator>),
            )
        }
    }
    #[derive(Clone)]
    struct Harness {
        text: Rc<std::cell::Cell<Option<Signal<String>>>>,
        initial: String,
    }
    fn harness(props: Harness) -> Element {
        let text = use_signal(|| props.initial);
        props.text.set(Some(text));
        rsx! {Markdown {text:text()}}
    }
    async fn until(dom: &mut VirtualDom, predicate: impl Fn(&VirtualDom) -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
                if predicate(dom) {
                    return;
                }
                dom.wait_for_work().await;
            }
        })
        .await
        .expect("mounted markdown milestone");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mounted_code_copy_wrap_and_streaming_fence_controls_preserve_identity() {
        use crate::runtime::transport_tests::{click_control, control, rendered_text};
        let props = Harness {
            text: Default::default(),
            initial: "```text title=stream.txt\nFirst code block\n```\n\nReply".into(),
        };
        let clipboard = Rc::new(std::cell::RefCell::new(Clipboard::default()));
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.provide_root_context(Rc::new(CopyDocument {
            owner: Default::default(),
            clipboard: clipboard.clone(),
        }) as Rc<dyn document::Document>);
        dom.rebuild_in_place();
        let copy_id = control(&dom, "Copy code").unwrap().0;
        click_control(&mut dom, "Disable line wrap");
        assert!(control(&dom, "Wrap lines").is_some());
        click_control(&mut dom, "Copy code");
        assert_eq!(clipboard.borrow().writes, ["First code block\n"]);
        assert!(
            control(&dom, "Copied").is_none(),
            "copy label waits for actual receipt"
        );
        let mut text = props.text.get().unwrap();
        for index in 0..10 {
            text.set(format!("{} {index}", props.initial));
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
            assert_eq!(control(&dom, "Copy code").unwrap().0, copy_id);
            assert!(
                control(&dom, "Wrap lines").is_some(),
                "stream suffix does not reset local wrap"
            );
        }
        text.set(
            props
                .initial
                .replace("First code block", "Updated code block"),
        );
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        {
            let mut clipboard = clipboard.borrow_mut();
            clipboard.allowed = true;
            for (_, waker) in std::mem::take(&mut clipboard.wakers) {
                waker.wake();
            }
        }
        until(&mut dom, |dom| control(dom, "Copied").is_some()).await;
        assert_eq!(control(&dom, "Copied").unwrap().0, copy_id);
        click_control(&mut dom, "Copied");
        until(&mut dom, |dom| {
            rendered_text(dom).contains("Updated code block")
        })
        .await;
        assert_eq!(
            clipboard.borrow().writes,
            ["First code block\n", "Updated code block\n"]
        );
        assert!(control(&dom, "Wrap lines").is_some());
        // Closing an initially open fence must not replace its controls either.
        text.set("```text\nOpen fence".into());
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        let open_id = control(&dom, "Copied").unwrap().0;
        text.set("```text\nOpen fence\n```\n\nFollowing sibling".into());
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert_eq!(control(&dom, "Copied").unwrap().0, open_id);
        assert!(control(&dom, "Wrap lines").is_some());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn newest_copy_receipt_owns_success_failure_and_label() {
        use crate::runtime::transport_tests::{click_control, control, rendered_text};
        let clipboard = Rc::new(std::cell::RefCell::new(Clipboard::default()));
        let mut dom = VirtualDom::new_with_props(
            harness,
            Harness {
                text: Default::default(),
                initial: "```text\ncopy me\n```".into(),
            },
        );
        dom.provide_root_context(Rc::new(CopyDocument {
            owner: Default::default(),
            clipboard: clipboard.clone(),
        }) as Rc<dyn document::Document>);
        dom.rebuild_in_place();
        // An old failure after a newer success must not replace its receipt UI.
        click_control(&mut dom, "Copy code");
        click_control(&mut dom, "Copy code");
        clipboard.borrow_mut().complete(1, true);
        until(&mut dom, |dom| control(dom, "Copied").is_some()).await;
        clipboard.borrow_mut().complete(0, false);
        until(&mut dom, |_| clipboard.borrow().observed.contains(&0)).await;
        assert!(control(&dom, "Copied").is_some());
        assert!(!rendered_text(&dom).contains("Could not copy"));
        // Conversely an old success must not clear the newer failure.
        click_control(&mut dom, "Copied");
        click_control(&mut dom, "Copied");
        clipboard.borrow_mut().complete(3, false);
        until(&mut dom, |dom| {
            rendered_text(dom).contains("Could not copy")
        })
        .await;
        clipboard.borrow_mut().complete(2, true);
        until(&mut dom, |_| clipboard.borrow().observed.contains(&2)).await;
        assert!(rendered_text(&dom).contains("Could not copy"));
        assert_eq!(clipboard.borrow().observed.len(), 4);
    }
}
