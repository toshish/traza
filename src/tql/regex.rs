//! The small regular-expression engine behind TQL's `~` operator.
//!
//! Std only and linear time: a pattern compiles to a Thompson NFA that is
//! simulated breadth-first over the text, one character at a time, so the
//! cost of a match is bounded by `text length × program size` whatever the
//! pattern — there is no backtracking for a hostile pattern to explode. The
//! engine only answers "does this text contain a match?"; TQL never needs
//! the match's position or its groups.
//!
//! **The subset** (documented in `docs/guide/tql.md`):
//!
//! | Syntax | Meaning |
//! |---|---|
//! | `abc` | literal characters |
//! | `.` | any character except a newline |
//! | `[abc]` `[a-z]` `[^…]` | a character class, ranges and negation |
//! | `\d \w \s` / `\D \W \S` | digit, word character, whitespace / their complements (also inside classes) |
//! | `*` `+` `?` `{n}` `{n,}` `{n,m}` | repetition (a trailing `?` is accepted and changes nothing: only "is there a match" is asked) |
//! | `a\|b` | alternation |
//! | `( … )` `(?: … )` | grouping (no captures) |
//! | `^` `$` | start and end of a LINE (multi-line: TQL text fields join several values with newlines) |
//! | `\A` `\z` | start and end of the whole text |
//! | `\b` `\B` | word boundary / not a word boundary |
//! | `(?i)` | at the very start: ASCII and simple Unicode case-insensitive matching |
//! | `\n \t \r \f \v` `\xHH` `\u{H…}` | control characters and code points |
//! | `\` + punctuation | that punctuation, literally |
//!
//! Not supported, and refused with the column of the construct rather than
//! silently misread: backreferences, lookaround, named or capturing group
//! syntax other than `(?:`, inline flags other than a leading `(?i)`, POSIX
//! classes (`[:alpha:]`), and Unicode classes (`\p{…}`).

use std::fmt;
use std::sync::Arc;

use super::meter::Meter;

/// Largest compiled program a pattern may produce. Repetition counts expand
/// the program, so this bounds both memory and the per-character cost.
pub(crate) const MAX_PROGRAM: usize = 4096;

/// Largest repetition count in `{n,m}`.
pub(crate) const MAX_REPEAT: u32 = 256;

/// Longest pattern accepted, in characters.
pub(crate) const MAX_PATTERN_CHARS: usize = 1024;

/// A compiled pattern. Cheap to clone: the program is shared.
#[derive(Clone)]
pub(crate) struct Regex {
    source: Arc<str>,
    inner: Arc<Compiled>,
}

struct Compiled {
    program: Vec<Inst>,
    case_insensitive: bool,
    /// A pattern that is one literal string runs as a substring search.
    literal: Option<String>,
}

impl PartialEq for Regex {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source
    }
}

impl fmt::Debug for Regex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Regex({:?})", self.source)
    }
}

/// Why a pattern was refused, and at which character of the pattern (from
/// zero).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegexError {
    pub(crate) message: String,
    pub(crate) offset: usize,
}

impl Regex {
    /// Compiles `pattern`.
    pub(crate) fn new(pattern: &str) -> Result<Self, RegexError> {
        let chars: Vec<char> = pattern.chars().collect();
        if chars.len() > MAX_PATTERN_CHARS {
            return Err(RegexError {
                message: format!("the regex is longer than {MAX_PATTERN_CHARS} characters"),
                offset: 0,
            });
        }
        let mut parser = Parser {
            chars: &chars,
            at: 0,
        };
        let case_insensitive = parser.eat_str("(?i)");
        let node = parser.alternation()?;
        if parser.at < chars.len() {
            // Only an unbalanced ')' stops the top-level alternation early.
            return Err(RegexError {
                message: "unbalanced `)`".to_owned(),
                offset: parser.at,
            });
        }
        let literal = match (&node, case_insensitive) {
            (_, true) => None,
            (Node::Char(c), false) => Some(c.to_string()),
            (Node::Concat(items), false) if items.iter().all(|n| matches!(n, Node::Char(_))) => {
                Some(
                    items
                        .iter()
                        .filter_map(|n| match n {
                            Node::Char(c) => Some(*c),
                            _ => None,
                        })
                        .collect(),
                )
            }
            _ => None,
        };
        let mut compiler = Compiler {
            program: Vec::new(),
            case_insensitive,
        };
        compiler
            .emit(&node)
            .map_err(|message| RegexError { message, offset: 0 })?;
        compiler.push(Inst::Match)?;
        Ok(Self {
            source: Arc::from(pattern),
            inner: Arc::new(Compiled {
                program: compiler.program,
                case_insensitive,
                literal,
            }),
        })
    }

    /// The pattern as written.
    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    /// Instructions in the compiled program: the per-character cost bound a
    /// query's regex budget counts.
    pub(crate) fn size(&self) -> usize {
        self.inner.program.len()
    }

    /// Whether `text` contains a match. The work — `text length × live
    /// instructions` — is charged to `meter`, so a heavy pattern over a long
    /// text stops at the query deadline rather than past it.
    pub(crate) fn is_match(&self, text: &str, meter: &mut Meter) -> crate::Result<bool> {
        let compiled = &*self.inner;
        if let Some(literal) = &compiled.literal {
            meter.charge(text.len() as u64 / LITERAL_BYTES_PER_UNIT + 1)?;
            return Ok(text.contains(literal.as_str()));
        }
        simulate(compiled, text, meter)
    }
}

/// Bytes a substring search covers per unit of work.
const LITERAL_BYTES_PER_UNIT: u64 = 4;

/// Units a simulation accumulates before charging them, so the meter is not
/// consulted on every character.
const CHARGE_BATCH: u64 = 1 << 14;

// ------------------------------------------------------------------ syntax

#[derive(Clone, Debug)]
enum Node {
    Empty,
    Char(char),
    Any,
    Class(Class),
    Assert(Assertion),
    Concat(Vec<Node>),
    Alternate(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Clone, Debug)]
struct Class {
    ranges: Vec<(char, char)>,
    negated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Assertion {
    LineStart,
    LineEnd,
    TextStart,
    TextEnd,
    WordBoundary,
    NotWordBoundary,
}

struct Parser<'p> {
    chars: &'p [char],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, expected: &str) -> bool {
        let wanted: Vec<char> = expected.chars().collect();
        if self.chars[self.at..].starts_with(&wanted) {
            self.at += wanted.len();
            true
        } else {
            false
        }
    }

    fn error<T>(&self, offset: usize, message: impl Into<String>) -> Result<T, RegexError> {
        Err(RegexError {
            message: message.into(),
            offset,
        })
    }

    fn alternation(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.concatenation()?];
        while self.eat('|') {
            branches.push(self.concatenation()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alternate(branches)
        })
    }

    fn concatenation(&mut self) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            items.push(self.repetition()?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn repetition(&mut self) -> Result<Node, RegexError> {
        let node = self.atom()?;
        let at = self.at;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.at += 1;
                (0, None)
            }
            Some('+') => {
                self.at += 1;
                (1, None)
            }
            Some('?') => {
                self.at += 1;
                (0, Some(1))
            }
            Some('{') => match self.counted()? {
                Some(bounds) => bounds,
                None => return Ok(node),
            },
            _ => return Ok(node),
        };
        if matches!(node, Node::Assert(_) | Node::Empty) {
            return self.error(at, "a repetition needs something to repeat");
        }
        // A lazy marker is accepted and means nothing to a yes/no match.
        self.eat('?');
        let second = self.at;
        let doubled = match self.peek() {
            Some('*' | '+' | '?') => true,
            Some('{') => self.counted()?.is_some(),
            _ => false,
        };
        if doubled {
            return self.error(
                second,
                "a repetition cannot directly follow another; group it",
            );
        }
        Ok(Node::Repeat {
            node: Box::new(node),
            min,
            max,
        })
    }

    /// `{n}`, `{n,}`, `{n,m}` at the cursor, or `None` (cursor unmoved) when
    /// the brace does not open a count — then it is a literal `{`.
    fn counted(&mut self) -> Result<Option<(u32, Option<u32>)>, RegexError> {
        let open = self.at;
        let mut cursor = self.at + 1;
        let read = |cursor: &mut usize| -> Option<u32> {
            let begin = *cursor;
            while self.chars.get(*cursor).is_some_and(char::is_ascii_digit) {
                *cursor += 1;
            }
            (begin < *cursor)
                .then(|| self.chars[begin..*cursor].iter().collect::<String>())
                .and_then(|digits| digits.parse().ok())
        };
        let Some(min) = read(&mut cursor) else {
            return Ok(None);
        };
        let max = if self.chars.get(cursor) == Some(&',') {
            cursor += 1;
            read(&mut cursor)
        } else {
            Some(min)
        };
        if self.chars.get(cursor) != Some(&'}') {
            return Ok(None);
        }
        let unbounded_comma = self.chars.get(cursor - 1) == Some(&',');
        let max = if unbounded_comma { None } else { max };
        if min > MAX_REPEAT || max.is_some_and(|max| max > MAX_REPEAT) {
            return self.error(
                open,
                format!("repetition counts are capped at {MAX_REPEAT}"),
            );
        }
        if max.is_some_and(|max| max < min) {
            return self.error(open, "a repetition's maximum is below its minimum");
        }
        self.at = cursor + 1;
        Ok(Some((min, max)))
    }

    fn atom(&mut self) -> Result<Node, RegexError> {
        let at = self.at;
        let Some(c) = self.peek() else {
            return self.error(at, "unexpected end of the regex");
        };
        self.at += 1;
        match c {
            '(' => {
                if self.eat('?') && !self.eat(':') {
                    return self.error(at, "only `(?:` groups and a leading `(?i)` are supported");
                }
                let inner = self.alternation()?;
                if !self.eat(')') {
                    return self.error(at, "unclosed `(`");
                }
                Ok(inner)
            }
            ')' => self.error(at, "unbalanced `)`"),
            '[' => self.class(at),
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Assert(Assertion::LineStart)),
            '$' => Ok(Node::Assert(Assertion::LineEnd)),
            '*' | '+' | '?' => self.error(at, "a repetition needs something to repeat"),
            '\\' => self.escape(at, false).map(|item| match item {
                Escaped::Char(c) => Node::Char(c),
                Escaped::Class(class) => Node::Class(class),
                Escaped::Assert(assertion) => Node::Assert(assertion),
            }),
            other => Ok(Node::Char(other)),
        }
    }

    fn escape(&mut self, at: usize, in_class: bool) -> Result<Escaped, RegexError> {
        let Some(c) = self.peek() else {
            return self.error(at, "a trailing `\\` escapes nothing");
        };
        self.at += 1;
        let class = |ranges: &[(char, char)], negated: bool| {
            Escaped::Class(Class {
                ranges: ranges.to_vec(),
                negated,
            })
        };
        Ok(match c {
            'd' => class(DIGIT, false),
            'D' => class(DIGIT, true),
            'w' => class(WORD, false),
            'W' => class(WORD, true),
            's' => class(SPACE, false),
            'S' => class(SPACE, true),
            'n' => Escaped::Char('\n'),
            't' => Escaped::Char('\t'),
            'r' => Escaped::Char('\r'),
            'f' => Escaped::Char('\u{c}'),
            'v' => Escaped::Char('\u{b}'),
            'x' => {
                let digits: String = self.chars[self.at..].iter().take(2).collect();
                if digits.len() != 2 || !digits.chars().all(|d| d.is_ascii_hexdigit()) {
                    return self.error(at, "`\\x` takes two hex digits");
                }
                self.at += 2;
                Escaped::Char(char::from(u8::from_str_radix(&digits, 16).unwrap_or(0)))
            }
            'u' => {
                if !self.eat('{') {
                    return self.error(at, "write a code point as `\\u{…}`");
                }
                let begin = self.at;
                while self.peek().is_some_and(|d| d.is_ascii_hexdigit()) {
                    self.at += 1;
                }
                let digits: String = self.chars[begin..self.at].iter().collect();
                if !self.eat('}') || digits.is_empty() || digits.len() > 6 {
                    return self.error(at, "write a code point as `\\u{…}`");
                }
                match u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    Some(c) => Escaped::Char(c),
                    None => return self.error(at, "not a Unicode scalar value"),
                }
            }
            'b' if !in_class => Escaped::Assert(Assertion::WordBoundary),
            'B' if !in_class => Escaped::Assert(Assertion::NotWordBoundary),
            'A' if !in_class => Escaped::Assert(Assertion::TextStart),
            'z' if !in_class => Escaped::Assert(Assertion::TextEnd),
            'p' | 'P' => return self.error(at, "Unicode classes (`\\p{…}`) are not supported"),
            digit if digit.is_ascii_digit() => {
                return self.error(at, "backreferences are not supported")
            }
            letter if letter.is_alphanumeric() => {
                return self.error(at, format!("unknown escape `\\{letter}`"))
            }
            other => Escaped::Char(other),
        })
    }

    fn class(&mut self, open: usize) -> Result<Node, RegexError> {
        let negated = self.eat('^');
        let mut ranges = Vec::new();
        let mut negated_parts: Vec<Class> = Vec::new();
        let mut first = true;
        loop {
            let at = self.at;
            let Some(c) = self.peek() else {
                return self.error(open, "unclosed `[`");
            };
            if c == ']' && !first {
                self.at += 1;
                break;
            }
            first = false;
            if c == '[' && self.chars.get(self.at + 1) == Some(&':') {
                return self.error(at, "POSIX classes (`[:alpha:]`) are not supported");
            }
            self.at += 1;
            let low = match c {
                '\\' => match self.escape(at, true)? {
                    Escaped::Char(c) => c,
                    Escaped::Class(class) => {
                        if class.negated {
                            negated_parts.push(class);
                        } else {
                            ranges.extend(class.ranges);
                        }
                        continue;
                    }
                    Escaped::Assert(_) => return self.error(at, "an assertion inside a class"),
                },
                other => other,
            };
            // A range `a-z`, unless the `-` is last (then it is literal).
            if self.peek() == Some('-')
                && self.chars.get(self.at + 1).is_some_and(|next| *next != ']')
            {
                self.at += 1;
                let high_at = self.at;
                let Some(high) = self.peek() else {
                    return self.error(open, "unclosed `[`");
                };
                self.at += 1;
                let high = if high == '\\' {
                    match self.escape(high_at, true)? {
                        Escaped::Char(c) => c,
                        _ => return self.error(high_at, "a range must end at a character"),
                    }
                } else {
                    high
                };
                if high < low {
                    return self.error(at, "a class range runs backwards");
                }
                ranges.push((low, high));
            } else {
                ranges.push((low, low));
            }
        }
        if negated_parts.is_empty() {
            return Ok(Node::Class(Class { ranges, negated }));
        }
        // `[\D…]`: a negated escape inside a class. Expressed as an
        // alternation of the positive part and each complement, then negated
        // as a whole when the class was.
        let mut branches: Vec<Node> = negated_parts.into_iter().map(Node::Class).collect();
        if !ranges.is_empty() {
            branches.push(Node::Class(Class {
                ranges,
                negated: false,
            }));
        }
        if negated {
            return self.error(open, "a negated class cannot contain `\\D`, `\\W` or `\\S`");
        }
        Ok(Node::Alternate(branches))
    }
}

enum Escaped {
    Char(char),
    Class(Class),
    Assert(Assertion),
}

const DIGIT: &[(char, char)] = &[('0', '9')];
const WORD: &[(char, char)] = &[('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];
const SPACE: &[(char, char)] = &[('\t', '\r'), (' ', ' ')];

// ---------------------------------------------------------------- program

#[derive(Clone, Debug)]
enum Inst {
    Char(char),
    Any,
    Class(Box<Class>),
    Assert(Assertion),
    Split(usize, usize),
    Jmp(usize),
    Match,
}

struct Compiler {
    program: Vec<Inst>,
    case_insensitive: bool,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize, RegexError> {
        if self.program.len() >= MAX_PROGRAM {
            return Err(RegexError {
                message: format!(
                    "the regex expands past {MAX_PROGRAM} instructions; lower its repetition counts"
                ),
                offset: 0,
            });
        }
        self.program.push(inst);
        Ok(self.program.len() - 1)
    }

    fn emit(&mut self, node: &Node) -> Result<(), String> {
        self.emit_inner(node).map_err(|error| error.message)
    }

    fn emit_inner(&mut self, node: &Node) -> Result<(), RegexError> {
        match node {
            Node::Empty => {}
            Node::Char(c) => {
                let c = if self.case_insensitive { fold(*c) } else { *c };
                self.push(Inst::Char(c))?;
            }
            Node::Any => {
                self.push(Inst::Any)?;
            }
            Node::Class(class) => {
                self.push(Inst::Class(Box::new(class.clone())))?;
            }
            Node::Assert(assertion) => {
                self.push(Inst::Assert(*assertion))?;
            }
            Node::Concat(items) => {
                for item in items {
                    self.emit_inner(item)?;
                }
            }
            Node::Alternate(branches) => {
                let mut jumps = Vec::new();
                for (index, branch) in branches.iter().enumerate() {
                    if index + 1 < branches.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit_inner(branch)?;
                        jumps.push(self.push(Inst::Jmp(0))?);
                        let next = self.program.len();
                        self.program[split] = Inst::Split(split + 1, next);
                    } else {
                        self.emit_inner(branch)?;
                    }
                }
                let end = self.program.len();
                for jump in jumps {
                    self.program[jump] = Inst::Jmp(end);
                }
            }
            Node::Repeat { node, min, max } => {
                for _ in 0..*min {
                    self.emit_inner(node)?;
                }
                match max {
                    None => {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit_inner(node)?;
                        self.push(Inst::Jmp(split))?;
                        let end = self.program.len();
                        self.program[split] = Inst::Split(split + 1, end);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.push(Inst::Split(0, 0))?);
                            self.emit_inner(node)?;
                        }
                        let end = self.program.len();
                        for split in splits {
                            self.program[split] = Inst::Split(split + 1, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Simple case folding: a character's lowercase form when it has exactly
/// one, else itself.
fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

fn unfold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == '_')
}

impl Class {
    fn contains(&self, c: char, case_insensitive: bool) -> bool {
        let hit = |c: char| {
            self.ranges
                .iter()
                .any(|(low, high)| *low <= c && c <= *high)
        };
        let found = hit(c) || (case_insensitive && (hit(fold(c)) || hit(unfold(c))));
        found != self.negated
    }
}

// -------------------------------------------------------------- simulation

/// A set of program counters with O(1) insert, test and clear.
struct PcSet {
    dense: Vec<usize>,
    stamp: Vec<u32>,
    generation: u32,
}

impl PcSet {
    fn new(size: usize) -> Self {
        Self {
            dense: Vec::with_capacity(size),
            stamp: vec![0; size],
            generation: 1,
        }
    }

    fn clear(&mut self) {
        self.dense.clear();
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.stamp.iter_mut().for_each(|slot| *slot = 0);
            self.generation = 1;
        }
    }

    fn insert(&mut self, pc: usize) -> bool {
        if self.stamp[pc] == self.generation {
            return false;
        }
        self.stamp[pc] = self.generation;
        self.dense.push(pc);
        true
    }
}

/// Adds `pc` and everything reachable from it without consuming a character
/// to `set`, given the characters either side of the current position.
/// Returns whether `Match` was reached.
fn closure(
    program: &[Inst],
    set: &mut PcSet,
    stack: &mut Vec<usize>,
    pc: usize,
    before: Option<char>,
    after: Option<char>,
) -> bool {
    let mut matched = false;
    stack.push(pc);
    while let Some(pc) = stack.pop() {
        if !set.insert(pc) {
            continue;
        }
        match &program[pc] {
            Inst::Split(first, second) => {
                stack.push(*second);
                stack.push(*first);
            }
            Inst::Jmp(target) => stack.push(*target),
            Inst::Assert(assertion) => {
                let holds = match assertion {
                    Assertion::LineStart => before.is_none() || before == Some('\n'),
                    Assertion::LineEnd => after.is_none() || after == Some('\n'),
                    Assertion::TextStart => before.is_none(),
                    Assertion::TextEnd => after.is_none(),
                    Assertion::WordBoundary => is_word(before) != is_word(after),
                    Assertion::NotWordBoundary => is_word(before) == is_word(after),
                };
                if holds {
                    stack.push(pc + 1);
                }
            }
            Inst::Match => matched = true,
            Inst::Char(_) | Inst::Any | Inst::Class(_) => {}
        }
    }
    matched
}

fn simulate(compiled: &Compiled, text: &str, meter: &mut Meter) -> crate::Result<bool> {
    let program = &compiled.program;
    let mut current = PcSet::new(program.len());
    let mut next = PcSet::new(program.len());
    let mut pending: Vec<usize> = Vec::new();
    let mut stack = Vec::new();
    let mut before: Option<char> = None;
    let mut chars = text.chars().peekable();
    // Setting up the two sets is proportional to the program.
    let mut unbilled = program.len() as u64;
    loop {
        let after = chars.peek().copied();
        current.clear();
        // Threads carried over from the previous character, then a new
        // thread starting here: the search is unanchored.
        for pc in pending.drain(..) {
            if closure(program, &mut current, &mut stack, pc, before, after) {
                meter.charge(unbilled)?;
                return Ok(true);
            }
        }
        if closure(program, &mut current, &mut stack, 0, before, after) {
            meter.charge(unbilled)?;
            return Ok(true);
        }
        // This character's cost: every state the closures reached.
        unbilled += current.dense.len() as u64 + 1;
        if unbilled >= CHARGE_BATCH {
            meter.charge(unbilled)?;
            unbilled = 0;
        }
        let Some(c) = after else {
            meter.charge(unbilled)?;
            return Ok(false);
        };
        chars.next();
        let folded = if compiled.case_insensitive {
            fold(c)
        } else {
            c
        };
        next.clear();
        for &pc in &current.dense {
            let advances = match &program[pc] {
                Inst::Char(expected) => *expected == folded,
                Inst::Any => c != '\n',
                Inst::Class(class) => class.contains(c, compiled.case_insensitive),
                _ => false,
            };
            if advances && next.insert(pc + 1) {
                pending.push(pc + 1);
            }
        }
        before = Some(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        Regex::new(pattern)
            .unwrap_or_else(|error| panic!("{pattern}: {error:?}"))
            .is_match(text, &mut Meter::new(None))
            .expect("no deadline")
    }

    #[test]
    fn literals_search_anywhere() {
        assert!(matches("pytest", "run pytest -q"));
        assert!(!matches("pytest", "run py.test"));
        assert!(matches("", "anything"));
        assert!(matches("a.c", "xxabcxx"));
        assert!(!matches("a.c", "a\nc"), "dot does not match a newline");
    }

    #[test]
    fn classes_ranges_and_escapes() {
        assert!(matches("[0-9]+ passed", "12 passed"));
        assert!(!matches("^[0-9]+ passed", "x12 passed"));
        assert!(matches("[^a-z]", "abc1"));
        assert!(!matches("^[^a-z]+$", "abc"));
        assert!(matches(r"\d\d", "a42"));
        assert!(matches(r"[\d_]x", "_x"));
        assert!(matches(r"test_\w+\.py", "tests/test_login.py"));
        assert!(matches(r"\s", "a b"));
        assert!(!matches(r"\S", "  \t"));
        assert!(matches(r"[-a]", "-"));
        assert!(matches(r"[a-]", "-"));
        assert!(matches(r"[]a]", "]"));
        assert!(matches(r"\.", "a.b"));
        assert!(!matches(r"\.", "ab"));
        assert!(matches(r"\x41", "A"));
        assert!(matches(r"\u{e9}", "café"));
        assert!(matches(r"[\D]", "a"));
        assert!(!matches(r"^[\D]+$", "a1"));
    }

    #[test]
    fn repetition_and_alternation() {
        assert!(matches("ab*c", "ac"));
        assert!(matches("ab+c", "abbbc"));
        assert!(!matches("ab+c", "ac"));
        assert!(matches("colou?r", "color"));
        assert!(matches("^a{3}$", "aaa"));
        assert!(!matches("^a{3}$", "aaaa"));
        assert!(matches("^a{2,}$", "aaaaa"));
        assert!(matches("^a{2,3}$", "aa"));
        assert!(!matches("^a{2,3}$", "a"));
        assert!(matches("^(ab|cd)+$", "abcdab"));
        assert!(!matches("^(ab|cd)+$", "abc"));
        assert!(
            matches("x{", "x{"),
            "a brace that opens no count is literal"
        );
        assert!(matches("a+?b", "aab"), "a lazy marker is accepted");
        assert!(matches("(?:ab)*c", "ababc"));
        assert!(matches("(a*)*b", "aaab"), "empty loops terminate");
    }

    #[test]
    fn anchors_are_line_anchors_and_text_anchors_exist() {
        assert!(matches("^pytest", "cd repo\npytest -x"));
        assert!(matches("done$", "step one\ndone\nmore"));
        assert!(!matches(r"\Apytest", "cd repo\npytest -x"));
        assert!(matches(r"repo\z", "cd repo"));
        assert!(!matches(r"cd\z", "cd repo"));
        assert!(matches(r"\btest\b", "run the test now"));
        assert!(!matches(r"\btest\b", "pytest"));
        assert!(matches(r"\Btest", "pytest"));
    }

    #[test]
    fn case_insensitive_flag() {
        assert!(matches("(?i)permission denied", "PERMISSION DENIED"));
        assert!(matches("(?i)[a-c]x", "BX"));
        assert!(!matches("permission denied", "PERMISSION DENIED"));
        assert!(matches("(?i)ÉTÉ", "été"));
    }

    #[test]
    fn unsupported_constructs_are_refused_with_their_offset() {
        for (pattern, offset, needle) in [
            ("(?=a)", 0, "only `(?:`"),
            ("ab\\1", 2, "backreferences"),
            ("[[:alpha:]]", 1, "POSIX"),
            ("\\p{L}", 0, "Unicode classes"),
            ("a**", 2, "cannot directly follow"),
            ("(ab", 0, "unclosed `(`"),
            ("ab)", 2, "unbalanced"),
            ("[ab", 0, "unclosed `[`"),
            ("*a", 0, "needs something"),
            ("a{5,2}", 1, "below its minimum"),
            ("a{999}", 1, "capped"),
            ("\\q", 0, "unknown escape"),
            ("ab\\", 2, "trailing"),
            ("[z-a]", 1, "backwards"),
        ] {
            match Regex::new(pattern) {
                Err(error) => {
                    assert!(error.message.contains(needle), "{pattern}: {error:?}");
                    assert_eq!(error.offset, offset, "{pattern}: {error:?}");
                }
                Ok(_) => panic!("{pattern} must be refused"),
            }
        }
    }

    #[test]
    fn programs_are_bounded() {
        let error = Regex::new("(a{200}){200}").expect_err("refused");
        assert!(error.message.contains("expands past"), "{error:?}");
        let long = "a".repeat(MAX_PATTERN_CHARS + 1);
        assert!(Regex::new(&long).is_err());
    }

    #[test]
    fn the_work_is_charged_and_the_deadline_honoured() {
        let text = "a".repeat(64 * 1024);
        let heavy = Regex::new(r"(?:[^\x00]{255}){16}\x00").expect("compiles");
        assert!(heavy.size() > 4000);
        // Charged by characters × live instructions, not by calls.
        let light = Regex::new("a+b").expect("compiles");
        let mut meter = Meter::new(None);
        assert!(!light.is_match(&text, &mut meter).expect("no deadline"));
        assert!(meter.spent() >= text.len() as u64, "{}", meter.spent());
        let literal = Regex::new("needle").expect("compiles");
        let mut meter = Meter::new(None);
        assert!(!literal.is_match(&text, &mut meter).expect("no deadline"));
        assert!(meter.spent() >= text.len() as u64 / LITERAL_BYTES_PER_UNIT);
        // A spent deadline stops a heavy match partway instead of after it.
        let expired =
            crate::Deadline::starting(std::time::Instant::now(), Some(std::time::Duration::ZERO));
        let mut meter = Meter::new(expired);
        match heavy.is_match(&text, &mut meter) {
            Err(crate::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
        assert!(
            meter.spent() < 1 << 20,
            "stopped within a check of the deadline, not at the end ({})",
            meter.spent()
        );
    }

    #[test]
    fn linear_time_on_a_pathological_pattern() {
        // `(a|a)*b` against a long run of a's is exponential for a naive
        // backtracker; here it is one pass.
        let text = "a".repeat(20_000);
        let started = std::time::Instant::now();
        assert!(!matches("(a|aa)*b", &text));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A brute-force reference for the same subset (minus classes and
    /// assertions), by recursive backtracking over every split, against which
    /// random patterns and texts are checked.
    fn reference(node: &Node, text: &[char], at: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        match node {
            Node::Empty => k(at),
            Node::Char(c) => text.get(at) == Some(c) && k(at + 1),
            Node::Any => text.get(at).is_some_and(|c| *c != '\n') && k(at + 1),
            Node::Concat(items) => {
                fn chain(
                    items: &[Node],
                    text: &[char],
                    at: usize,
                    k: &mut dyn FnMut(usize) -> bool,
                ) -> bool {
                    match items.split_first() {
                        None => k(at),
                        Some((first, rest)) => {
                            reference(first, text, at, &mut |next| chain(rest, text, next, k))
                        }
                    }
                }
                chain(items, text, at, k)
            }
            Node::Alternate(branches) => branches.iter().any(|b| reference(b, text, at, k)),
            Node::Repeat { node, min, max } => {
                fn repeat(
                    node: &Node,
                    min: u32,
                    max: Option<u32>,
                    count: u32,
                    text: &[char],
                    at: usize,
                    k: &mut dyn FnMut(usize) -> bool,
                ) -> bool {
                    if count >= min && k(at) {
                        return true;
                    }
                    if max.is_some_and(|max| count >= max) {
                        return false;
                    }
                    // An iteration matching nothing still counts toward the
                    // minimum; past it, only progress may loop.
                    reference(node, text, at, &mut |next| {
                        (next > at || count < min)
                            && repeat(node, min, max, count + 1, text, next, k)
                    })
                }
                repeat(node, *min, *max, 0, text, at, k)
            }
            Node::Class(_) | Node::Assert(_) => unreachable!("not generated"),
        }
    }

    #[test]
    fn random_patterns_agree_with_a_backtracking_reference() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        fn pattern(next: &mut dyn FnMut(u64) -> u64, depth: u32) -> String {
            let leaf = |next: &mut dyn FnMut(u64) -> u64| match next(4) {
                0 => "a".to_owned(),
                1 => "b".to_owned(),
                2 => ".".to_owned(),
                _ => "ab".to_owned(),
            };
            if depth == 0 {
                return leaf(next);
            }
            match next(6) {
                0 => format!("{}{}", pattern(next, depth - 1), pattern(next, depth - 1)),
                1 => format!(
                    "({}|{})",
                    pattern(next, depth - 1),
                    pattern(next, depth - 1)
                ),
                2 => format!("({})*", pattern(next, depth - 1)),
                3 => format!("({})?", pattern(next, depth - 1)),
                4 => format!("({}){{1,2}}", pattern(next, depth - 1)),
                _ => leaf(next),
            }
        }
        for _ in 0..3000 {
            let source = pattern(&mut next, 3);
            let length = next(7) as usize;
            let text: String = (0..length)
                .map(|_| if next(3) == 0 { 'b' } else { 'a' })
                .collect();
            let regex = Regex::new(&source).expect("generated patterns compile");
            let chars: Vec<char> = source.chars().collect();
            let node = Parser {
                chars: &chars,
                at: 0,
            }
            .alternation()
            .expect("parses");
            let text_chars: Vec<char> = text.chars().collect();
            let expected = (0..=text_chars.len())
                .any(|start| reference(&node, &text_chars, start, &mut |_| true));
            assert_eq!(
                regex
                    .is_match(&text, &mut Meter::new(None))
                    .expect("no deadline"),
                expected,
                "{source} on {text:?}"
            );
        }
    }
}
