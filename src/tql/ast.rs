//! TQL's syntax tree, its canonical rendering, and the static facts the
//! compiler and `explain` read off it.

use std::fmt::Write as _;

use super::regex::Regex;

/// A pattern over a run's steps.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    /// One step satisfying the atom.
    Atom(Box<Atom>),
    /// The steps between the two sides of a `..>` link: any number (within
    /// the bounds) of steps satisfying the gap's predicate, matched lazily
    /// and not reported as part of the match's spans.
    Gap(Box<Gap>),
    /// The parts in order, each starting where the last ended.
    Concat(Vec<Node>),
    /// The first alternative that leads to a match, in order.
    Alternate(Vec<Node>),
    /// `node` repeated `min..=max` times, adjacent; greedy prefers more.
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greedy: bool,
    },
    /// Zero-width: the position before the run's first step.
    Start,
    /// Zero-width: the position after the run's last step.
    End,
}

/// The step kinds an atom can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AtomKind {
    /// Any step.
    Step,
    /// A tool step.
    Tool,
    /// A model call.
    Llm,
    /// An agent invocation.
    Agent,
    /// A tool step that edits a file (see the `edit` field).
    Edit,
    /// A tool step that runs a test suite (see the `test` field).
    Test,
}

impl AtomKind {
    pub(crate) fn keyword(self) -> &'static str {
        match self {
            Self::Step => "step",
            Self::Tool => "tool",
            Self::Llm => "llm",
            Self::Agent => "agent",
            Self::Edit => "edit",
            Self::Test => "test",
        }
    }

    pub(crate) const ALL: [AtomKind; 6] = [
        Self::Step,
        Self::Tool,
        Self::Llm,
        Self::Agent,
        Self::Edit,
        Self::Test,
    ];
}

/// One step's test: its kind, optional name alternatives, and predicates
/// (every bracket must hold).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Atom {
    pub(crate) kind: AtomKind,
    /// Empty means any name.
    pub(crate) names: Vec<NameTest>,
    pub(crate) preds: Vec<Pred>,
}

/// One alternative in an atom's name list.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NameTest {
    /// Equal, ignoring ASCII case.
    Exact(String),
    /// The regex finds a match in the name.
    Regex(Regex),
    /// The name is captured (`bind`) or compared with a capture.
    Capture { slot: usize, bind: bool },
}

/// The gap of a `..>` link.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Gap {
    pub(crate) pred: Option<Pred>,
    pub(crate) min: u32,
    pub(crate) max: Option<u32>,
}

/// A boolean condition on one step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Pred {
    And(Vec<Pred>),
    Or(Vec<Pred>),
    Not(Box<Pred>),
    Cond(Cond),
}

/// `field`, `field OP value`, or a binding `field = @capture`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cond {
    pub(crate) field: Field,
    pub(crate) test: Test,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Test {
    /// The field is present and truthy.
    Truthy,
    Compare(Op, Literal),
    /// `field = @slot` at a binding position: binds when the capture is
    /// unbound (one alternative per value of a multi-valued field), and
    /// compares when it is bound.
    Bind(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Eq,
    Ne,
    Match,
    NotMatch,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Op {
    pub(crate) fn symbol(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Match => "~",
            Self::NotMatch => "!~",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Literal {
    Str(String),
    Num(f64),
    /// Nanoseconds, written with a unit.
    Duration(u64),
    Bool(bool),
    Regex(Regex),
    /// A capture's value, optionally scaled (`2*@c`).
    Capture {
        slot: usize,
        factor: Option<f64>,
    },
}

/// What a predicate reads from a step. See the field table in
/// `docs/guide/tql.md`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Field {
    Kind,
    Name,
    Span,
    Service,
    Agent,
    Model,
    Tool,
    Status,
    Error,
    Cost,
    Duration,
    Tokens,
    InputTokens,
    OutputTokens,
    CacheRead,
    CacheWrite,
    Context,
    Index,
    Args,
    Result,
    ArgsPath(Vec<String>),
    ResultPath(Vec<String>),
    File,
    Digest,
    Fingerprint,
    Signature,
    Calls,
    Offered,
    InCatalog,
    UnknownCalls,
    Edit,
    TestRun,
    Attr(String),
}

/// What kind of value a field yields, for the parser's type checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldType {
    Text,
    Number,
    Duration,
    Bool,
    List,
    /// A JSON value whose type is known only per step (`args.X`, `attr.X`).
    Dynamic,
}

/// The named fields: name, constructor, type, one-line meaning. The single
/// table behind parsing, suggestions, `explain`, and the guide's field list.
pub(crate) const FIELDS: &[(&str, FieldType, &str)] = &[
    ("kind", FieldType::Text, "llm, tool or agent"),
    ("name", FieldType::Text, "the tool, model or agent name (else the span name)"),
    ("span", FieldType::Text, "the span's operation name"),
    ("service", FieldType::Text, "the emitting service"),
    ("agent", FieldType::Text, "the agent the step ran under: its own agent name, else the nearest enclosing agent span's"),
    ("model", FieldType::Text, "the model, when the span names one"),
    ("tool", FieldType::Text, "the tool name, for tool steps"),
    ("status", FieldType::Text, "the span status as recorded"),
    ("error", FieldType::Bool, "the status is `error` (any case)"),
    ("cost", FieldType::Number, "USD, metered or priced from the server's rates"),
    ("duration", FieldType::Duration, "end minus start; compare with a unit (30s, 500ms)"),
    ("tokens", FieldType::Number, "total tokens reported"),
    ("input_tokens", FieldType::Number, "prompt tokens reported"),
    ("output_tokens", FieldType::Number, "completion tokens reported"),
    ("cache_read", FieldType::Number, "prompt tokens read from a provider cache"),
    ("cache_write", FieldType::Number, "prompt tokens written to a provider cache"),
    ("context", FieldType::Number, "context tokens the call carried, where that can be told"),
    ("index", FieldType::Number, "the step's position in the run, from 0"),
    ("args", FieldType::Text, "the step's input as text (alias `input`)"),
    ("result", FieldType::Text, "the step's output as text (alias `output`)"),
    ("file", FieldType::List, "paths named by a tool step's arguments"),
    ("digest", FieldType::Text, "SHA-256 of the step's stored input"),
    ("fingerprint", FieldType::Text, "kind:name#hash, the repeat identity"),
    ("signature", FieldType::Text, "kind:name"),
    ("calls", FieldType::List, "tool names an llm step's response asked for"),
    ("offered", FieldType::List, "the tool catalog an llm step offered (for a tool step: its requesting llm step's)"),
    ("in_catalog", FieldType::Bool, "a tool step's name is in its requesting llm step's catalog (absent when the catalog is unknown)"),
    ("unknown_calls", FieldType::List, "calls an llm step made to tools outside its own catalog"),
    ("edit", FieldType::Bool, "a tool step that edits a file"),
    ("test", FieldType::Bool, "a tool step whose input runs a test suite"),
];

impl Field {
    /// A plain (non-path) field by name, with its type.
    pub(crate) fn named(name: &str) -> Option<(Field, FieldType)> {
        let field = match name {
            "kind" => Self::Kind,
            "name" => Self::Name,
            "span" => Self::Span,
            "service" => Self::Service,
            "agent" => Self::Agent,
            "model" => Self::Model,
            "tool" => Self::Tool,
            "status" => Self::Status,
            "error" => Self::Error,
            "cost" => Self::Cost,
            "duration" => Self::Duration,
            "tokens" => Self::Tokens,
            "input_tokens" => Self::InputTokens,
            "output_tokens" => Self::OutputTokens,
            "cache_read" => Self::CacheRead,
            "cache_write" => Self::CacheWrite,
            "context" => Self::Context,
            "index" => Self::Index,
            "args" | "input" => Self::Args,
            "result" | "output" => Self::Result,
            "file" => Self::File,
            "digest" => Self::Digest,
            "fingerprint" => Self::Fingerprint,
            "signature" => Self::Signature,
            "calls" => Self::Calls,
            "offered" => Self::Offered,
            "in_catalog" => Self::InCatalog,
            "unknown_calls" => Self::UnknownCalls,
            "edit" => Self::Edit,
            "test" => Self::TestRun,
            _ => return None,
        };
        let ty = field.field_type();
        Some((field, ty))
    }

    pub(crate) fn field_type(&self) -> FieldType {
        match self {
            Self::Kind
            | Self::Name
            | Self::Span
            | Self::Service
            | Self::Agent
            | Self::Model
            | Self::Tool
            | Self::Status
            | Self::Args
            | Self::Result
            | Self::Digest
            | Self::Fingerprint
            | Self::Signature => FieldType::Text,
            Self::Error | Self::InCatalog | Self::Edit | Self::TestRun => FieldType::Bool,
            Self::Cost
            | Self::Tokens
            | Self::InputTokens
            | Self::OutputTokens
            | Self::CacheRead
            | Self::CacheWrite
            | Self::Context
            | Self::Index => FieldType::Number,
            Self::Duration => FieldType::Duration,
            Self::File | Self::Calls | Self::Offered | Self::UnknownCalls => FieldType::List,
            Self::ArgsPath(_) | Self::ResultPath(_) | Self::Attr(_) => FieldType::Dynamic,
        }
    }

    /// The canonical spelling.
    pub(crate) fn render(&self) -> String {
        let path = |head: &str, segments: &[String]| {
            let mut out = head.to_owned();
            for (index, segment) in segments.iter().enumerate() {
                out.push('.');
                if index + 1 == segments.len() && !is_path_segment(segment) {
                    out.push_str(&quote(segment));
                } else {
                    out.push_str(segment);
                }
            }
            out
        };
        match self {
            Self::ArgsPath(segments) => path("args", segments),
            Self::ResultPath(segments) => path("result", segments),
            Self::Attr(key) => {
                if is_attr_key(key) {
                    format!("attr.{key}")
                } else {
                    format!("attr.{}", quote(key))
                }
            }
            other => FIELDS
                .iter()
                .find(|(name, ..)| Field::named(name).is_some_and(|(field, _)| &field == other))
                .map(|(name, ..)| (*name).to_owned())
                .unwrap_or_default(),
        }
    }
}

/// Whether `segment` can be written bare in a dotted path.
pub(crate) fn is_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether an attribute key can follow `attr.` bare: path segments joined
/// by single dots.
fn is_attr_key(key: &str) -> bool {
    !key.is_empty() && key.split('.').all(is_path_segment) && !key.ends_with('-')
}

/// Words the parser reserves inside predicates and after a pattern.
pub(crate) const KEYWORDS: [&str; 6] = ["and", "or", "not", "true", "false", "within"];

/// Whether `text` lexes back as one bare identifier with exactly this text.
pub(crate) fn is_bare_word(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let Some(first) = chars.first() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || *first == '_') {
        return false;
    }
    for (index, c) in chars.iter().enumerate().skip(1) {
        let next = chars.get(index + 1);
        let ok = match c {
            c if c.is_ascii_alphanumeric() => true,
            '_' | ':' | '/' => true,
            '-' => next != Some(&'>'),
            '.' => next != Some(&'.') && next.is_some(),
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    !KEYWORDS
        .iter()
        .any(|keyword| keyword.eq_ignore_ascii_case(text))
}

/// A string literal that lexes back to exactly `text`.
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn word_or_quote(text: &str) -> String {
    if is_bare_word(text) {
        text.to_owned()
    } else {
        quote(text)
    }
}

/// A duration in the largest unit that divides it exactly.
pub(crate) fn render_duration(nanos: u64) -> String {
    const UNITS: [(u64, &str); 7] = [
        (86_400_000_000_000, "d"),
        (3_600_000_000_000, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "us"),
        (1, "ns"),
    ];
    if nanos == 0 {
        return "0s".to_owned();
    }
    for (size, unit) in UNITS {
        if nanos % size == 0 {
            return format!("{}{unit}", nanos / size);
        }
    }
    format!("{nanos}ns")
}

fn render_number(number: f64) -> String {
    format!("{number}")
}

/// Renders a pattern in canonical form: the grouping is explicit wherever
/// precedence decided it, so the rendering shows how a query was read, and
/// parsing the rendering yields the same tree.
pub(crate) fn render(node: &Node, captures: &[String]) -> String {
    let mut out = String::new();
    render_node(node, captures, &mut out);
    out
}

fn render_node(node: &Node, captures: &[String], out: &mut String) {
    match node {
        Node::Atom(atom) => render_atom(atom, captures, out),
        Node::Gap(gap) => render_gap(gap, captures, out),
        Node::Concat(items) => {
            let mut previous: Option<&Node> = None;
            for item in items {
                if previous.is_some() && !matches!(item, Node::Gap(_)) && starts_with_gap(item) {
                    out.push(' ');
                    render_continuation(item, captures, out);
                    previous = Some(item);
                    continue;
                }
                if let Some(previous) = previous {
                    let linked = !matches!(previous, Node::Gap(_) | Node::Start)
                        && !matches!(item, Node::Gap(_) | Node::End);
                    if linked {
                        out.push_str(" -> ");
                    } else if matches!(item, Node::Gap(_)) || matches!(previous, Node::Gap(_)) {
                        out.push(' ');
                    }
                }
                let grouped = matches!(item, Node::Concat(_) | Node::Alternate(_));
                if grouped {
                    out.push('(');
                }
                render_node(item, captures, out);
                if grouped {
                    out.push(')');
                }
                previous = Some(item);
            }
        }
        Node::Alternate(branches) => {
            for (index, branch) in branches.iter().enumerate() {
                if index > 0 {
                    out.push_str(" | ");
                }
                let grouped = matches!(branch, Node::Alternate(_));
                if grouped {
                    out.push('(');
                }
                render_node(branch, captures, out);
                if grouped {
                    out.push(')');
                }
            }
        }
        Node::Repeat {
            node,
            min,
            max,
            greedy,
        } => {
            let grouped = !matches!(**node, Node::Atom(_));
            if grouped {
                out.push('(');
            }
            render_node(node, captures, out);
            if grouped {
                out.push(')');
            }
            out.push_str(&render_bounds(*min, *max, true));
            if !greedy {
                out.push('?');
            }
        }
        Node::Start => out.push('^'),
        Node::End => out.push('$'),
    }
}

/// Whether a pattern's first step is reached through a gap — which, inside
/// a sequence, means it was written as a continuation group
/// (`A (..> B){2}`). An alternation qualifies when any branch does.
fn starts_with_gap(node: &Node) -> bool {
    match node {
        Node::Gap(_) => true,
        Node::Concat(items) => items.first().is_some_and(starts_with_gap),
        Node::Repeat { node, .. } => starts_with_gap(node),
        Node::Alternate(branches) => branches.iter().any(starts_with_gap),
        Node::Atom(_) | Node::Start | Node::End => false,
    }
}

/// A continuation group: `(..> B){n}`, `(-> B | ..> C)`.
fn render_continuation(node: &Node, captures: &[String], out: &mut String) {
    let body = match node {
        Node::Repeat { node, .. } => &**node,
        other => other,
    };
    out.push('(');
    let branches: Vec<&Node> = match body {
        Node::Alternate(branches) => branches.iter().collect(),
        other => vec![other],
    };
    for (index, branch) in branches.into_iter().enumerate() {
        if index > 0 {
            out.push_str(" | ");
        }
        if !starts_with_gap(branch) {
            out.push_str("-> ");
            let grouped = matches!(branch, Node::Alternate(_));
            if grouped {
                out.push('(');
            }
            render_node(branch, captures, out);
            if grouped {
                out.push(')');
            }
        } else {
            render_node(branch, captures, out);
        }
    }
    out.push(')');
    if let Node::Repeat {
        min, max, greedy, ..
    } = node
    {
        out.push_str(&render_bounds(*min, *max, true));
        if !greedy {
            out.push('?');
        }
    }
}

/// `*`, `+`, `?` or `{…}` for a quantifier (`shorthand`), `{…}` only for a
/// gap.
fn render_bounds(min: u32, max: Option<u32>, shorthand: bool) -> String {
    match (min, max, shorthand) {
        (0, None, true) => "*".to_owned(),
        (1, None, true) => "+".to_owned(),
        (0, Some(1), true) => "?".to_owned(),
        (min, Some(max), _) if min == max => format!("{{{min}}}"),
        (0, Some(max), false) => format!("{{,{max}}}"),
        (min, Some(max), _) => format!("{{{min},{max}}}"),
        (min, None, _) => format!("{{{min},}}"),
    }
}

fn render_gap(gap: &Gap, captures: &[String], out: &mut String) {
    out.push_str("..");
    if let Some(pred) = &gap.pred {
        out.push('[');
        render_pred(pred, captures, out, false);
        out.push(']');
    }
    if (gap.min, gap.max) != (0, None) {
        out.push_str(&render_bounds(gap.min, gap.max, false));
    }
    out.push('>');
}

fn render_atom(atom: &Atom, captures: &[String], out: &mut String) {
    out.push_str(atom.kind.keyword());
    if !atom.names.is_empty() {
        out.push('(');
        for (index, name) in atom.names.iter().enumerate() {
            if index > 0 {
                out.push('|');
            }
            match name {
                NameTest::Exact(text) => out.push_str(&word_or_quote(text)),
                NameTest::Regex(regex) => {
                    out.push('~');
                    out.push_str(&quote(regex.source()));
                }
                NameTest::Capture { slot, .. } => {
                    let _ = write!(out, "@{}", captures[*slot]);
                }
            }
        }
        out.push(')');
    }
    for pred in &atom.preds {
        out.push('[');
        render_pred(pred, captures, out, false);
        out.push(']');
    }
}

fn render_pred(pred: &Pred, captures: &[String], out: &mut String, nested: bool) {
    match pred {
        Pred::And(items) => {
            if nested {
                out.push('(');
            }
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(" and ");
                }
                render_pred(
                    item,
                    captures,
                    out,
                    matches!(item, Pred::Or(_) | Pred::And(_)),
                );
            }
            if nested {
                out.push(')');
            }
        }
        Pred::Or(items) => {
            if nested {
                out.push('(');
            }
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(" or ");
                }
                render_pred(item, captures, out, matches!(item, Pred::Or(_)));
            }
            if nested {
                out.push(')');
            }
        }
        Pred::Not(inner) => {
            out.push_str("not ");
            render_pred(
                inner,
                captures,
                out,
                matches!(**inner, Pred::And(_) | Pred::Or(_)),
            );
        }
        Pred::Cond(cond) => {
            out.push_str(&cond.field.render());
            match &cond.test {
                Test::Truthy => {}
                Test::Bind(slot) => {
                    let _ = write!(out, "=@{}", captures[*slot]);
                }
                Test::Compare(op, literal) => {
                    out.push_str(op.symbol());
                    render_literal(literal, captures, out);
                }
            }
        }
    }
}

fn render_literal(literal: &Literal, captures: &[String], out: &mut String) {
    match literal {
        Literal::Str(text) => out.push_str(&word_or_quote(text)),
        Literal::Num(number) => out.push_str(&render_number(*number)),
        Literal::Duration(nanos) => out.push_str(&render_duration(*nanos)),
        Literal::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Literal::Regex(regex) => out.push_str(&quote(regex.source())),
        Literal::Capture { slot, factor } => {
            if let Some(factor) = factor {
                let _ = write!(out, "{}*", render_number(*factor));
            }
            let _ = write!(out, "@{}", captures[*slot]);
        }
    }
}

/// The fewest and most steps `node` can consume (`None`: unbounded).
pub(crate) fn step_bounds(node: &Node) -> (u64, Option<u64>) {
    match node {
        Node::Atom(_) => (1, Some(1)),
        Node::Gap(gap) => (u64::from(gap.min), gap.max.map(u64::from)),
        Node::Start | Node::End => (0, Some(0)),
        Node::Concat(items) => items.iter().fold((0, Some(0)), |(min, max), item| {
            let (item_min, item_max) = step_bounds(item);
            (
                min.saturating_add(item_min),
                max.zip(item_max).map(|(a, b)| a.saturating_add(b)),
            )
        }),
        Node::Alternate(branches) => {
            let bounds: Vec<_> = branches.iter().map(step_bounds).collect();
            let min = bounds.iter().map(|(min, _)| *min).min().unwrap_or(0);
            let max = bounds
                .iter()
                .try_fold(0_u64, |acc, (_, max)| max.map(|max| acc.max(max)));
            (min, max)
        }
        Node::Repeat { node, min, max, .. } => {
            let (inner_min, inner_max) = step_bounds(node);
            let low = inner_min.saturating_mul(u64::from(*min));
            let high = match (inner_max, max) {
                (Some(0), _) => Some(0),
                (Some(inner), Some(max)) => Some(inner.saturating_mul(u64::from(*max))),
                _ => None,
            };
            (low, high)
        }
    }
}

/// Every field a pattern reads, in first-use order, rendered.
pub(crate) fn fields_used(node: &Node) -> Vec<String> {
    fn pred_fields(pred: &Pred, out: &mut Vec<String>) {
        match pred {
            Pred::And(items) | Pred::Or(items) => {
                items.iter().for_each(|item| pred_fields(item, out))
            }
            Pred::Not(inner) => pred_fields(inner, out),
            Pred::Cond(cond) => {
                let name = cond.field.render();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    fn walk(node: &Node, out: &mut Vec<String>) {
        match node {
            Node::Atom(atom) => atom.preds.iter().for_each(|pred| pred_fields(pred, out)),
            Node::Gap(gap) => {
                if let Some(pred) = &gap.pred {
                    pred_fields(pred, out);
                }
            }
            Node::Concat(items) | Node::Alternate(items) => {
                items.iter().for_each(|item| walk(item, out))
            }
            Node::Repeat { node, .. } => walk(node, out),
            Node::Start | Node::End => {}
        }
    }
    let mut out = Vec::new();
    walk(node, &mut out);
    out
}

/// Atoms in a pattern (counting each written atom once).
pub(crate) fn atom_count(node: &Node) -> usize {
    match node {
        Node::Atom(_) => 1,
        Node::Gap(_) | Node::Start | Node::End => 0,
        Node::Concat(items) | Node::Alternate(items) => items.iter().map(atom_count).sum(),
        Node::Repeat { node, .. } => atom_count(node),
    }
}

/// The closest name to `wanted` among `candidates`, when one is close
/// enough to be a likely typo.
pub(crate) fn suggest<'c>(
    wanted: &str,
    candidates: impl Iterator<Item = &'c str>,
) -> Option<&'c str> {
    let mut best: Option<(&str, usize)> = None;
    for candidate in candidates {
        let distance = edit_distance(wanted, candidate);
        if distance <= 2.max(wanted.len() / 3) && best.map_or(true, |(_, d)| distance < d) {
            best = Some((candidate, distance));
        }
    }
    best.map(|(candidate, _)| candidate)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()]
}
