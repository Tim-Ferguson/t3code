//! What the composer's `/`, `$`, `#` and `@` menus offer and in what order, ported from the
//! original (`composer-logic.ts`, `composerSlashCommandSearch.ts`, `providerSkillSearch.ts` and
//! `packages/shared/src/searchRanking.ts`) and replayed against it in
//! `tests/composer_menu_parity.rs`. Offsets are byte offsets into the prompt.
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Path,
    PullRequest,
    SlashCommand,
    Skill,
}
impl TriggerKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::PullRequest => "pull-request",
            Self::SlashCommand => "slash-command",
            Self::Skill => "skill",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub query: String,
    pub range_start: usize,
    pub range_end: usize,
}

fn clamp(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

/// `detectComposerTrigger`: the menu the text before the cursor asks for.
pub fn detect_trigger(text: &str, cursor: usize) -> Option<Trigger> {
    static PULL: OnceLock<Regex> = OnceLock::new();
    let cursor = clamp(text, cursor);
    let line_start = text[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let prefix = &text[line_start..cursor];
    if prefix.starts_with('/') && !prefix[1..].chars().any(char::is_whitespace) {
        return Some(Trigger { kind: TriggerKind::SlashCommand, query: prefix[1..].to_owned(), range_start: line_start, range_end: cursor });
    }
    let token_start = text[..cursor].rfind([' ', '\n', '\t', '\r']).map_or(0, |at| at + 1);
    let token = &text[token_start..cursor];
    let pull = PULL.get_or_init(|| Regex::new(r"^#([\p{L}\p{N}][\p{L}\p{N}_-]*)?$").expect("pull request token"));
    if let Some(captures) = pull.captures(token) {
        return Some(Trigger { kind: TriggerKind::PullRequest, query: captures.get(1).map_or("", |m| m.as_str()).to_owned(), range_start: token_start, range_end: cursor });
    }
    if let Some(first) = token.chars().next().filter(|c| is_currency(*c)) {
        return Some(Trigger { kind: TriggerKind::Skill, query: token[first.len_utf8()..].to_owned(), range_start: token_start, range_end: cursor });
    }
    let query = token.strip_prefix('@')?;
    Some(Trigger { kind: TriggerKind::Path, query: query.to_owned(), range_start: token_start, range_end: cursor })
}
fn is_currency(c: char) -> bool {
    static CURRENCY: OnceLock<Regex> = OnceLock::new();
    CURRENCY.get_or_init(|| Regex::new(r"^\p{Sc}$").expect("currency")).is_match(c.encode_utf8(&mut [0; 4]))
}

/// `replaceTextRange`: the prompt with a range replaced, and where the cursor lands.
pub fn replace_range(text: &str, start: usize, end: usize, replacement: &str) -> (String, usize) {
    let start = clamp(text, start);
    let end = clamp(text, end).max(start);
    (format!("{}{}{}", &text[..start], replacement, &text[end..]), start + replacement.len())
}

/// `parseStandaloneComposerSlashCommand`: a prompt that is only `/plan` or `/default`.
pub fn standalone_command(text: &str) -> Option<&'static str> {
    match text.trim().to_lowercase().as_str() {
        "/plan" => Some("plan"),
        "/default" => Some("default"),
        _ => None,
    }
}

// ---- Ranking (`searchRanking.ts`). ----

fn subsequence(value: &str, query: &str) -> Option<f64> {
    if query.is_empty() {
        return Some(0.);
    }
    let query: Vec<char> = query.chars().collect();
    let (mut at, mut first, mut previous, mut gaps) = (0usize, None::<usize>, None::<usize>, 0usize);
    let length = value.chars().count();
    for (index, c) in value.chars().enumerate() {
        if c != query[at] {
            continue;
        }
        first.get_or_insert(index);
        if let Some(previous) = previous {
            gaps += index - previous - 1;
        }
        previous = Some(index);
        at += 1;
        if at == query.len() {
            let first = first.unwrap_or(0);
            let span = index - first + 1 - query.len();
            let long = 64.min(length - query.len());
            return Some((first * 2 + gaps * 3 + span + long) as f64);
        }
    }
    None
}
fn long_penalty(value: &str, query: &str) -> f64 {
    64.min(value.chars().count().saturating_sub(query.chars().count())) as f64
}
struct Tiers<'a> {
    exact: f64,
    prefix: Option<f64>,
    boundary: Option<f64>,
    includes: Option<f64>,
    fuzzy: Option<f64>,
    markers: &'a [&'a str],
}
const DEFAULT_MARKERS: &[&str] = &[" ", "-", "_", "/"];
fn score(value: &str, query: &str, tiers: Tiers) -> Option<f64> {
    if value.is_empty() || query.is_empty() {
        return None;
    }
    if value == query {
        return Some(tiers.exact);
    }
    if let Some(base) = tiers.prefix.filter(|_| value.starts_with(query)) {
        return Some(base + long_penalty(value, query));
    }
    let index_of = |needle: &str| value.find(needle).map(|at| value[..at].chars().count());
    if let Some(base) = tiers.boundary {
        let best = tiers.markers.iter().filter_map(|marker| index_of(&format!("{marker}{query}")).map(|at| at + marker.chars().count())).min();
        if let Some(at) = best {
            return Some(base + (at * 2) as f64 + long_penalty(value, query));
        }
    }
    if let Some(base) = tiers.includes {
        if let Some(at) = index_of(query) {
            return Some(base + (at * 2) as f64 + long_penalty(value, query));
        }
    }
    tiers.fuzzy.and_then(|base| subsequence(value, query).map(|score| base + score))
}
/// The picker's token scoring (`scoreModelPickerSearchToken`): tiers offset by the field's base.
pub fn score_tiers(field: &str, token: &str, base: f64, fuzzy: bool) -> Option<f64> {
    score(field, token, Tiers { exact: base, prefix: Some(base + 2.), boundary: Some(base + 4.), includes: Some(base + 6.), fuzzy: fuzzy.then_some(base + 100.), markers: DEFAULT_MARKERS })
}
/// `normalizeSearchQuery`: trimmed, without the leading trigger characters, lowercase.
pub fn normalize_query(input: &str, leading: fn(char) -> bool) -> String {
    input.trim().trim_start_matches(leading).to_lowercase()
}

fn rank<T>(entries: Vec<(T, f64, String)>) -> Vec<T> {
    let mut entries: Vec<_> = entries.into_iter().enumerate().collect();
    entries.sort_by(|(left_index, left), (right_index, right)| left.1.partial_cmp(&right.1).unwrap().then_with(|| locale_compare(&left.2, &right.2)).then(left_index.cmp(right_index)));
    entries.into_iter().map(|(_, (item, _, _))| item).collect()
}
/// A close stand-in for `localeCompare` on the identifiers ranked here: letters before case, with
/// punctuation first.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let key = |text: &str| text.chars().map(|c| (c.is_alphanumeric(), c.to_lowercase().collect::<String>())).collect::<Vec<_>>();
    key(left).cmp(&key(right)).then_with(|| right.cmp(left))
}

// ---- Slash menu. ----

/// One row of the `/` menu.
#[derive(Debug, Clone, PartialEq)]
pub enum SlashItem {
    /// `/model`, `/plan`, `/default`: applied by the app.
    Command { command: String, description: String },
    /// A command the provider expands, only when it opens the whole message.
    Provider { name: String, description: String, provider: String },
    Skill { skill: Skill, provider: String },
}
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Skill {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub short_description: Option<String>,
    pub scope: Option<String>,
    pub enabled: bool,
    pub user_invocable: Option<bool>,
}
impl Skill {
    pub fn from_json(value: &Value) -> Option<Self> {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Some(Self {
            name: value["name"].as_str()?.to_owned(),
            display_name: text("displayName"),
            description: text("description"),
            short_description: text("shortDescription"),
            scope: text("scope"),
            enabled: value["enabled"].as_bool().unwrap_or(false),
            user_invocable: value["userInvocable"].as_bool(),
        })
    }
    pub fn invocable(&self) -> bool {
        self.enabled && self.user_invocable != Some(false)
    }
    /// `formatProviderSkillDisplayName`.
    pub fn label(&self) -> String {
        match self.display_name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => name.to_owned(),
            None => self.name.split(|c: char| c.is_whitespace() || matches!(c, ':' | '_' | '-')).filter(|word| !word.is_empty()).map(|word| word.chars().next().map(|first| first.to_uppercase().chain(word.chars().skip(1)).collect::<String>()).unwrap_or_default()).collect::<Vec<_>>().join(" "),
        }
    }
}

pub fn score_skill(skill: &Skill, query: &str) -> Option<f64> {
    let tier = |exact, prefix, boundary, includes, fuzzy, markers| Tiers { exact, prefix: Some(prefix), boundary: Some(boundary), includes: Some(includes), fuzzy, markers };
    let text = |value: &Option<String>| value.as_deref().unwrap_or("").to_lowercase();
    [
        score(&skill.name.to_lowercase(), query, tier(0., 2., 4., 6., Some(100.), &["-", "_", "/"])),
        score(&skill.label().to_lowercase(), query, tier(1., 3., 5., 7., Some(110.), DEFAULT_MARKERS)),
        score(&text(&skill.short_description), query, tier(20., 22., 24., 26., None, DEFAULT_MARKERS)),
        score(&text(&skill.description), query, tier(30., 32., 34., 36., None, DEFAULT_MARKERS)),
        score(&text(&skill.scope), query, tier(40., 42., 42., 44., None, DEFAULT_MARKERS)),
    ]
    .into_iter()
    .flatten()
    .min_by(|a, b| a.partial_cmp(b).unwrap())
}
fn is_currency_char(c: char) -> bool {
    is_currency(c)
}

/// `searchProviderSkills`: indexes into `skills`, best first. Skills that cannot be invoked are
/// left out and a name already listed is not offered twice.
pub fn search_skills(skills: &[Skill], query: &str) -> Vec<usize> {
    let mut seen = std::collections::BTreeSet::new();
    let usable: Vec<usize> = (0..skills.len()).filter(|index| skills[*index].invocable()).filter(|index| seen.insert(skills[*index].name.trim().to_lowercase())).collect();
    let query = normalize_query(query, is_currency_char);
    if query.is_empty() {
        return usable;
    }
    rank(usable.into_iter().filter_map(|index| Some((index, score_skill(&skills[index], &query)?, format!("{}\0{}", skills[index].label().to_lowercase(), skills[index].name)))).collect())
}

fn score_slash(item: &SlashItem, query: &str) -> Option<f64> {
    if let SlashItem::Skill { skill, .. } = item {
        if query == "skill" {
            return Some(0.);
        }
        let skill_query = query.strip_prefix("skill:").unwrap_or(query);
        let own = if skill_query.is_empty() { Some(0.) } else { score_skill(skill, skill_query) };
        return own.or_else(|| "skill".starts_with(query).then_some(9007199254740991.));
    }
    let (primary, description) = match item {
        SlashItem::Command { command, description } => (command.to_lowercase(), description.to_lowercase()),
        SlashItem::Provider { name, description, .. } => (name.to_lowercase(), description.to_lowercase()),
        SlashItem::Skill { .. } => unreachable!(),
    };
    let first = score(&primary, query, Tiers { exact: 0., prefix: Some(2.), boundary: Some(4.), includes: Some(6.), fuzzy: Some(100.), markers: &["-", "_", "/"] });
    let second = score(&description, query, Tiers { exact: 20., prefix: Some(22.), boundary: Some(24.), includes: Some(26.), fuzzy: None, markers: DEFAULT_MARKERS });
    [first, second].into_iter().flatten().min_by(|a, b| a.partial_cmp(b).unwrap())
}

/// `slashCommandItemsForPromptPosition`: provider commands only open a message.
pub fn items_for_position(items: &[SlashItem], at_prompt_start: bool) -> Vec<usize> {
    (0..items.len()).filter(|index| at_prompt_start || !matches!(items[*index], SlashItem::Provider { .. })).collect()
}

/// `searchSlashCommandItems`: indexes into `items`, best first.
pub fn search_slash(items: &[SlashItem], query: &str) -> Vec<usize> {
    let query = normalize_query(query, |c| c == '/');
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    rank(
        items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let tie = match item {
                    SlashItem::Command { command, .. } => format!("0\0{command}"),
                    SlashItem::Provider { name, provider, .. } => format!("1\0{name}\0{provider}"),
                    SlashItem::Skill { skill, provider } => format!("2\0{}\0{provider}", skill.name),
                };
                Some((index, score_slash(item, &query)?, tie))
            })
            .collect(),
    )
}
