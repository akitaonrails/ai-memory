//! Frontmatter YAML to JSON, resolved the way `serde_yaml` 0.9 resolved it.
//!
//! This exists for parity with existing hand-edited pages: a page that
//! indexed under `serde_yaml` 0.9 indexes to the same JSON now, except for
//! the YAML 1.1 shapes YAML 1.2 reads otherwise (a `#` glued to a token,
//! U+0085/U+2028/U+2029 as line breaks), pinned in `markdown.rs`'s tests.
//! The typed `serde-saphyr` deserializer reads some plain scalars
//! differently (`007` and `1_000` as numbers, `.inf` as an error) and fails
//! a page that uses `!!timestamp`, so this walks the parser events of
//! `serde-saphyr`'s own YAML parser and ports `serde_yaml` 0.9's scalar
//! rules. Frontmatter is untrusted, so nesting stays capped at
//! `serde_yaml`'s 128 containers and alias expansion at a fixed node count.

use std::collections::{HashMap, HashSet};
use std::num::ParseIntError;

use serde_json::{Map, Number, Value};
use serde_saphyr::granit_parser::{Event, Parser, ScalarStyle, Span, StrInput, Tag};

/// `serde_yaml` 0.9's recursion limit, in nested sequences and mappings.
const MAX_DEPTH: usize = 128;
/// Nodes one frontmatter block may expand to, aliases included: the
/// alias-bomb guard (`serde-saphyr`'s default `max_nodes`).
const MAX_NODES: usize = 250_000;
const CORE_TAG_PREFIX: &str = "tag:yaml.org,2002:";

/// Parse one YAML document into JSON. Empty input is `Null`.
pub(crate) fn parse(input: &str) -> Result<Value, String> {
    let mut walker = Walker {
        input,
        parser: Parser::new_from_str(input),
        anchors: HashMap::new(),
        nodes: 0,
    };
    walker.document()
}

/// An anchored node, kept for its aliases with the cost of replaying it.
struct Anchored {
    value: Value,
    nodes: usize,
    height: usize,
}

struct Walker<'a> {
    input: &'a str,
    parser: Parser<'a, StrInput<'a>>,
    anchors: HashMap<usize, Anchored>,
    nodes: usize,
}

impl<'a> Walker<'a> {
    fn next(&mut self) -> Result<(Event<'a>, Span), String> {
        loop {
            match self.parser.next_event() {
                Some(Ok((Event::Comment(..), _))) => {}
                Some(Ok(event)) => return Ok(event),
                Some(Err(error)) => return Err(error.to_string()),
                None => return Err("unexpected end of YAML input".to_owned()),
            }
        }
    }

    fn document(&mut self) -> Result<Value, String> {
        let mut value = Value::Null;
        let mut documents = 0;
        loop {
            match self.next()? {
                (Event::StreamStart | Event::DocumentEnd, _) => {}
                (Event::StreamEnd, _) => return Ok(value),
                (Event::DocumentStart(..), span) => {
                    documents += 1;
                    if documents > 1 {
                        return Err(format!(
                            "more than one YAML document is not supported{}",
                            at(span)
                        ));
                    }
                }
                (event, span) => value = self.node(event, span, 0)?.0,
            }
        }
    }

    /// Returns the node and its height in containers.
    fn node(
        &mut self,
        event: Event<'a>,
        span: Span,
        depth: usize,
    ) -> Result<(Value, usize), String> {
        let before = self.nodes;
        let (value, height, anchor, tag) = match event {
            Event::Alias(id) => {
                let anchored = self
                    .anchors
                    .get(&id)
                    .ok_or_else(|| format!("unknown anchor{}", at(span)))?;
                if depth + anchored.height > MAX_DEPTH {
                    return Err(format!("recursion limit exceeded{}", at(span)));
                }
                let (value, height, nodes) =
                    (anchored.value.clone(), anchored.height, anchored.nodes);
                self.count(nodes, span)?;
                return Ok((value, height));
            }
            Event::Scalar(text, style, anchor, tag) => {
                self.count(1, span)?;
                let tag = tag_string(tag.as_deref());
                // A tagged value's content ignores the tag it was taken by.
                let own_tag = if enum_tag(&tag).is_some() {
                    None
                } else {
                    tag.as_deref()
                };
                let mut value =
                    scalar(&text, style, own_tag).map_err(|e| format!("{e}{}", at(span)))?;
                // libyaml did not count a missing line break at the end of
                // input into a block scalar. Frontmatter always ends that way
                // (the fence drops the newline before `---`), so a block as
                // the last value lost its final newline under serde_yaml.
                if matches!(style, ScalarStyle::Literal | ScalarStyle::Folded)
                    && span.end.byte_offset() == Some(self.input.len())
                    && self.last_line_in_block(span)
                    && let Value::String(text) = &mut value
                    && text.ends_with('\n')
                {
                    text.pop();
                }
                (value, 0, anchor, tag)
            }
            Event::SequenceStart(_, anchor, tag) => {
                self.enter(depth, span)?;
                let mut items = Vec::new();
                let mut height = 0;
                loop {
                    match self.next()? {
                        (Event::SequenceEnd, _) => break,
                        (event, span) => {
                            let (item, h) = self.node(event, span, depth + 1)?;
                            height = height.max(h);
                            items.push(item);
                        }
                    }
                }
                (
                    Value::Array(items),
                    height + 1,
                    anchor,
                    tag_string(tag.as_deref()),
                )
            }
            Event::MappingStart(_, anchor, tag) => {
                self.enter(depth, span)?;
                let mut map = Map::new();
                let mut seen = HashSet::new();
                let mut height = 0;
                loop {
                    let (key, key_span) = match self.next()? {
                        (Event::MappingEnd, _) => break,
                        (event, span) => (self.node(event, span, depth + 1)?.0, span),
                    };
                    // serde_yaml compared keys as YAML values: `1` and `"1"`
                    // are different keys there, though both land on "1".
                    if !seen.insert(key.to_string()) {
                        return Err(format!("duplicate entry with key {key}{}", at(key_span)));
                    }
                    let key = json_key(key).map_err(|e| format!("{e}{}", at(key_span)))?;
                    let (event, span) = self.next()?;
                    let (value, h) = self.node(event, span, depth + 1)?;
                    height = height.max(h);
                    map.insert(key, value);
                }
                (
                    Value::Object(map),
                    height + 1,
                    anchor,
                    tag_string(tag.as_deref()),
                )
            }
            _ => return Err(format!("unexpected YAML event{}", at(span))),
        };
        // A `!local` tag became a one-entry map in serde_yaml's JSON.
        let value = match enum_tag(&tag) {
            Some(name) => Value::Object(Map::from_iter([(name, value)])),
            None => value,
        };
        if anchor != 0 {
            let anchored = Anchored {
                value: value.clone(),
                nodes: self.nodes - before,
                height,
            };
            self.anchors.insert(anchor, anchored);
        }
        Ok((value, height))
    }

    /// Whether the input's unterminated last line belongs to the block
    /// scalar at `span`, whose span starts at its content indentation: any
    /// text does, and so does a blank line indented at least that deep.
    fn last_line_in_block(&self, span: Span) -> bool {
        match self.input.rsplit_once('\n') {
            Some((_, last)) if !last.is_empty() => {
                last.bytes().any(|b| b != b' ') || last.len() >= span.start.col()
            }
            _ => false,
        }
    }

    fn enter(&mut self, depth: usize, span: Span) -> Result<(), String> {
        self.count(1, span)?;
        if depth >= MAX_DEPTH {
            return Err(format!("recursion limit exceeded{}", at(span)));
        }
        Ok(())
    }

    fn count(&mut self, nodes: usize, span: Span) -> Result<(), String> {
        self.nodes = self.nodes.saturating_add(nodes);
        if self.nodes > MAX_NODES {
            return Err(format!(
                "frontmatter expands to more than {MAX_NODES} YAML nodes{}",
                at(span)
            ));
        }
        Ok(())
    }
}

/// The tag as libyaml spelled it: `!local`, or a resolved URI such as
/// `tag:yaml.org,2002:int` for `!!int`.
fn tag_string(tag: Option<&Tag>) -> Option<String> {
    tag.map(|tag| format!("{}{}", tag.handle(), tag.suffix()))
}

/// serde_yaml turned a tag starting with `!` into a tagged value, which its
/// JSON conversion wrote as `{"!name": value}`.
fn enum_tag(tag: &Option<String>) -> Option<String> {
    let tag = tag.as_deref()?;
    let rest = tag.strip_prefix('!')?;
    let name = if rest.is_empty() { tag } else { rest };
    let unbanged = match name.strip_prefix('!') {
        Some("") | None => name,
        Some(unbanged) => unbanged,
    };
    Some(format!("!{unbanged}"))
}

/// A key as serde_json's map-key serializer accepted it.
fn json_key(key: Value) -> Result<String, String> {
    match key {
        Value::String(s) => Ok(s),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err("key must be a string".to_owned()),
    }
}

/// `serde_yaml` 0.9's `visit_scalar`.
fn scalar(text: &str, style: ScalarStyle, tag: Option<&str>) -> Result<Value, String> {
    let core = tag.and_then(|t| t.strip_prefix(CORE_TAG_PREFIX));
    match core {
        Some("bool") => parse_bool(text)
            .map(Value::Bool)
            .ok_or_else(|| format!("invalid value: `{text}`, expected a boolean")),
        Some("int") => {
            int(text)?.ok_or_else(|| format!("invalid value: `{text}`, expected an integer"))
        }
        Some("float") => parse_f64(text)
            .map(float)
            .ok_or_else(|| format!("invalid value: `{text}`, expected a float")),
        Some("null") => parse_null(text)
            .then_some(Value::Null)
            .ok_or_else(|| format!("invalid value: `{text}`, expected null")),
        // Any other tag (`!!str`, `!!timestamp`, `!!binary`) keeps the text.
        _ if tag.is_none() && style == ScalarStyle::Plain => untagged(text),
        _ => Ok(Value::String(text.to_owned())),
    }
}

/// `serde_yaml` 0.9's `visit_untagged_scalar`.
fn untagged(text: &str) -> Result<Value, String> {
    if text.is_empty() || parse_null(text) {
        return Ok(Value::Null);
    }
    if let Some(b) = parse_bool(text) {
        return Ok(Value::Bool(b));
    }
    if let Some(int) = int(text)? {
        return Ok(int);
    }
    if !digits_but_not_number(text)
        && let Some(f) = parse_f64(text)
    {
        return Ok(float(f));
    }
    Ok(Value::String(text.to_owned()))
}

/// JSON has no infinity or NaN; serde_json wrote them as null.
fn float(f: f64) -> Value {
    Number::from_f64(f).map_or(Value::Null, Value::Number)
}

/// `serde_yaml` 0.9's `visit_int`. An integer wider than 64 bits was an
/// error there (`serde_yaml::Value` has no room for it), and stays one.
fn int(text: &str) -> Result<Option<Value>, String> {
    if let Some(u) = parse_unsigned_int(text, u64::from_str_radix) {
        return Ok(Some(Value::from(u)));
    }
    if let Some(i) = parse_negative_int(text, i64::from_str_radix) {
        return Ok(Some(Value::from(i)));
    }
    if parse_unsigned_int(text, u128::from_str_radix).is_some()
        || parse_negative_int(text, i128::from_str_radix).is_some()
    {
        return Err(format!("integer `{text}` does not fit in 64 bits"));
    }
    Ok(None)
}

fn parse_null(text: &str) -> bool {
    matches!(text, "null" | "Null" | "NULL" | "~")
}

fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

type Radix<T> = fn(&str, u32) -> Result<T, ParseIntError>;

fn parse_unsigned_int<T>(text: &str, from_str_radix: Radix<T>) -> Option<T> {
    let unpositive = text.strip_prefix('+').unwrap_or(text);
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = unpositive.strip_prefix(prefix) {
            if rest.starts_with(['+', '-']) {
                return None;
            }
            if let Ok(int) = from_str_radix(rest, radix) {
                return Some(int);
            }
        }
    }
    if unpositive.starts_with(['+', '-']) || digits_but_not_number(text) {
        return None;
    }
    from_str_radix(unpositive, 10).ok()
}

fn parse_negative_int<T>(text: &str, from_str_radix: Radix<T>) -> Option<T> {
    for (prefix, radix) in [("-0x", 16), ("-0o", 8), ("-0b", 2)] {
        if let Some(rest) = text.strip_prefix(prefix)
            && let Ok(int) = from_str_radix(&format!("-{rest}"), radix)
        {
            return Some(int);
        }
    }
    if digits_but_not_number(text) {
        return None;
    }
    from_str_radix(text, 10).ok()
}

fn parse_f64(text: &str) -> Option<f64> {
    let unpositive = match text.strip_prefix('+') {
        Some(rest) if rest.starts_with(['+', '-']) => return None,
        Some(rest) => rest,
        None => text,
    };
    if let ".inf" | ".Inf" | ".INF" = unpositive {
        return Some(f64::INFINITY);
    }
    if let "-.inf" | "-.Inf" | "-.INF" = text {
        return Some(f64::NEG_INFINITY);
    }
    if let ".nan" | ".NaN" | ".NAN" = text {
        return Some(f64::NAN);
    }
    unpositive.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// A leading zero followed by digits is a string in YAML 1.2.
fn digits_but_not_number(text: &str) -> bool {
    let text = text.strip_prefix(['-', '+']).unwrap_or(text);
    text.len() > 1 && text.starts_with('0') && text[1..].bytes().all(|b| b.is_ascii_digit())
}

fn at(span: Span) -> String {
    format!(
        " at line {} column {}",
        span.start.line(),
        span.start.col() + 1
    )
}
