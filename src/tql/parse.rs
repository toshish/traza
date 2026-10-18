//! TQL's lexer and recursive-descent parser. Every refusal names the
//! 1-based character column it happened at.

use super::ast::{
    self, step_bounds, Atom, AtomKind, Cond, Field, FieldType, Gap, Literal, NameTest, Node, Op,
    Pred, Test,
};
use super::regex::Regex;
use super::{TqlError, Within};

/// Longest query accepted, in characters.
pub(crate) const MAX_QUERY_CHARS: usize = 4096;

/// Most distinct captures in one query.
pub(crate) const MAX_CAPTURES: usize = 16;

/// Largest count in a quantifier or a gap bound.
pub(crate) const MAX_COUNT: u32 = 100;

/// Deepest nesting of groups and predicates, so a hostile query cannot
/// exhaust the parser's stack.
const MAX_DEPTH: usize = 64;

/// Most atoms one query may write.
pub(crate) const MAX_ATOMS: usize = 256;

/// Compiled regex instructions one query may hold across all its regexes.
/// A regex costs `text length × instructions` per evaluation; this keeps
/// the per-step cost of a query to a few maximal regexes rather than a
/// hundred `or`-ed ones.
pub(crate) const MAX_QUERY_REGEX_INSTRUCTIONS: usize = 8192;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    /// A `/…/` literal: its source and whether it carried the `i` flag.
    Regex(String, bool),
    /// A number, and whether it was written as an integer.
    Num(f64, bool),
    Duration(u64),
    Capture(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Pipe,
    Star,
    Plus,
    Question,
    Caret,
    Dollar,
    Arrow,
    DotDot,
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
    Tilde,
    NotTilde,
    Eof,
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::Ident(text) => format!("`{text}`"),
            Tok::Str(_) => "a string".to_owned(),
            Tok::Regex(..) => "a regex".to_owned(),
            Tok::Num(..) => "a number".to_owned(),
            Tok::Duration(_) => "a duration".to_owned(),
            Tok::Capture(name) => format!("`@{name}`"),
            Tok::Eof => "the end of the query".to_owned(),
            other => format!("`{}`", other.symbol()),
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::Comma => ",",
            Tok::Pipe => "|",
            Tok::Star => "*",
            Tok::Plus => "+",
            Tok::Question => "?",
            Tok::Caret => "^",
            Tok::Dollar => "$",
            Tok::Arrow => "->",
            Tok::DotDot => "..",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Eq => "=",
            Tok::Ne => "!=",
            Tok::Tilde => "~",
            Tok::NotTilde => "!~",
            _ => "",
        }
    }
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    /// 1-based character column of the token's first character.
    col: usize,
}

fn error<T>(col: usize, message: impl Into<String>) -> Result<T, TqlError> {
    Err(TqlError {
        message: message.into(),
        column: col,
    })
}

// ------------------------------------------------------------------- lexer

fn lex(text: &str) -> Result<Vec<Token>, TqlError> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > MAX_QUERY_CHARS {
        return error(
            1,
            format!("the query is longer than {MAX_QUERY_CHARS} characters"),
        );
    }
    let mut tokens = Vec::new();
    let mut at = 0;
    let peek = |at: usize| chars.get(at).copied();
    while at < chars.len() {
        let c = chars[at];
        let col = at + 1;
        if c.is_whitespace() {
            at += 1;
            continue;
        }
        let single = |tok: Tok| Some((tok, 1));
        let simple =
            match c {
                '(' => single(Tok::LParen),
                ')' => single(Tok::RParen),
                '[' => single(Tok::LBracket),
                ']' => single(Tok::RBracket),
                '{' => single(Tok::LBrace),
                '}' => single(Tok::RBrace),
                ',' => single(Tok::Comma),
                '|' => single(Tok::Pipe),
                '*' => single(Tok::Star),
                '+' => single(Tok::Plus),
                '?' => single(Tok::Question),
                '^' => single(Tok::Caret),
                '$' => single(Tok::Dollar),
                '~' => single(Tok::Tilde),
                '>' if peek(at + 1) == Some('=') => Some((Tok::Ge, 2)),
                '>' => single(Tok::Gt),
                '<' if peek(at + 1) == Some('=') => Some((Tok::Le, 2)),
                '<' => single(Tok::Lt),
                '=' if peek(at + 1) == Some('=') => Some((Tok::Eq, 2)),
                '=' => single(Tok::Eq),
                '!' if peek(at + 1) == Some('=') => Some((Tok::Ne, 2)),
                '!' if peek(at + 1) == Some('~') => Some((Tok::NotTilde, 2)),
                '!' => return error(col, "write negation as `not`, `!=` or `!~`"),
                '-' if peek(at + 1) == Some('>') => Some((Tok::Arrow, 2)),
                '.' if peek(at + 1) == Some('.') => Some((Tok::DotDot, 2)),
                '.' => return error(
                    col,
                    "a lone `.`: links are `->` and `..>`, and numbers start with a digit (0.5)",
                ),
                _ => None,
            };
        if let Some((tok, width)) = simple {
            tokens.push(Token { tok, col });
            at += width;
            continue;
        }
        match c {
            '"' | '\'' => {
                let (value, end) = lex_string(&chars, at)?;
                tokens.push(Token {
                    tok: Tok::Str(value),
                    col,
                });
                at = end;
            }
            '/' => {
                let (source, insensitive, end) = lex_regex(&chars, at)?;
                tokens.push(Token {
                    tok: Tok::Regex(source, insensitive),
                    col,
                });
                at = end;
            }
            '@' => {
                let begin = at + 1;
                let mut end = begin;
                while chars
                    .get(end)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    end += 1;
                }
                let name: String = chars[begin..end].iter().collect();
                if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
                    return error(col, "a capture is `@` followed by a name, like `@file`");
                }
                tokens.push(Token {
                    tok: Tok::Capture(name),
                    col,
                });
                at = end;
            }
            '-' | '0'..='9' => {
                let (tok, end) = lex_number(&chars, at)?;
                tokens.push(Token { tok, col });
                at = end;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut end = at + 1;
                while let Some(&next) = chars.get(end) {
                    let after = chars.get(end + 1).copied();
                    let continues = match next {
                        c if c.is_ascii_alphanumeric() => true,
                        '_' | ':' | '/' => true,
                        '-' => after != Some('>'),
                        '.' => after != Some('.'),
                        _ => false,
                    };
                    if !continues {
                        break;
                    }
                    end += 1;
                }
                tokens.push(Token {
                    tok: Tok::Ident(chars[at..end].iter().collect()),
                    col,
                });
                at = end;
            }
            other => return error(col, format!("unexpected character `{other}`")),
        }
    }
    tokens.push(Token {
        tok: Tok::Eof,
        col: chars.len() + 1,
    });
    Ok(tokens)
}

/// A quoted string from `chars[open]` (a `"` or `'`): `\\`, `\"`, `\'`,
/// `\n`, `\t` and `\r` are escapes; any other backslash is kept as written,
/// so a regex's `\d` or `\.` reaches the regex engine intact.
fn lex_string(chars: &[char], open: usize) -> Result<(String, usize), TqlError> {
    let quote = chars[open];
    let mut value = String::new();
    let mut at = open + 1;
    loop {
        let Some(&c) = chars.get(at) else {
            return error(open + 1, "unterminated string");
        };
        at += 1;
        if c == quote {
            return Ok((value, at));
        }
        if c != '\\' {
            value.push(c);
            continue;
        }
        let Some(&escaped) = chars.get(at) else {
            return error(open + 1, "unterminated string");
        };
        at += 1;
        match escaped {
            '\\' => value.push('\\'),
            '"' => value.push('"'),
            '\'' => value.push('\''),
            'n' => value.push('\n'),
            't' => value.push('\t'),
            'r' => value.push('\r'),
            other => {
                value.push('\\');
                value.push(other);
            }
        }
    }
}

/// A `/…/flags` regex literal: verbatim, except that `\/` is a `/`.
fn lex_regex(chars: &[char], open: usize) -> Result<(String, bool, usize), TqlError> {
    let mut source = String::new();
    let mut at = open + 1;
    loop {
        let Some(&c) = chars.get(at) else {
            return error(open + 1, "unterminated regex: close it with `/`");
        };
        at += 1;
        match c {
            '/' => break,
            '\\' => {
                let Some(&escaped) = chars.get(at) else {
                    return error(open + 1, "unterminated regex: close it with `/`");
                };
                at += 1;
                if escaped != '/' {
                    source.push('\\');
                }
                source.push(escaped);
            }
            other => source.push(other),
        }
    }
    let mut insensitive = false;
    while let Some(&flag) = chars.get(at) {
        if !flag.is_ascii_alphabetic() {
            break;
        }
        if flag != 'i' {
            return error(
                at + 1,
                format!("unknown regex flag `{flag}`; only `i` is supported"),
            );
        }
        insensitive = true;
        at += 1;
    }
    Ok((source, insensitive, at))
}

/// Nanoseconds per duration unit.
fn duration_unit(unit: &str) -> Option<f64> {
    Some(match unit {
        "ns" => 1.0,
        "us" | "µs" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        "m" => 60e9,
        "h" => 3_600e9,
        "d" => 86_400e9,
        _ => return None,
    })
}

fn lex_number(chars: &[char], start: usize) -> Result<(Tok, usize), TqlError> {
    let mut at = start;
    if chars[at] == '-' {
        at += 1;
        if !chars.get(at).is_some_and(char::is_ascii_digit) {
            return error(start + 1, "a stray `-`: links are written `->`");
        }
    }
    while chars.get(at).is_some_and(char::is_ascii_digit) {
        at += 1;
    }
    let mut integer = true;
    if chars.get(at) == Some(&'.') && chars.get(at + 1).is_some_and(char::is_ascii_digit) {
        integer = false;
        at += 1;
        while chars.get(at).is_some_and(char::is_ascii_digit) {
            at += 1;
        }
    }
    let text: String = chars[start..at].iter().collect();
    let value: f64 = text.parse().map_err(|_| TqlError {
        message: format!("`{text}` is not a number"),
        column: start + 1,
    })?;
    let unit_start = at;
    while chars.get(at).is_some_and(|c| c.is_alphabetic()) {
        at += 1;
    }
    if unit_start == at {
        return Ok((Tok::Num(value, integer), at));
    }
    let unit: String = chars[unit_start..at].iter().collect();
    match duration_unit(&unit) {
        Some(scale) if value >= 0.0 => {
            let nanos = value * scale;
            if !nanos.is_finite() || nanos > u64::MAX as f64 {
                return error(start + 1, "that duration is too long");
            }
            Ok((Tok::Duration(nanos.round() as u64), at))
        }
        Some(_) => error(start + 1, "a duration cannot be negative"),
        None => error(
            unit_start + 1,
            format!(
                "unknown unit `{unit}` after a number (durations take ns, us, ms, s, m, h, d); \
                 quote a name that starts with a digit, like \"{text}{unit}\""
            ),
        ),
    }
}

// ------------------------------------------------------------------ parser

/// A parsed query before compilation.
pub(crate) struct Parsed {
    pub(crate) node: Node,
    pub(crate) within: Option<Within>,
    pub(crate) captures: Vec<String>,
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
    atoms: usize,
    captures: Vec<String>,
    /// Every capture reference: (slot, column), in order.
    references: Vec<(usize, usize)>,
    /// Regex instructions compiled so far.
    regex_instructions: usize,
}

/// Parses `text` into a syntax tree with captures resolved and binding
/// positions marked.
pub(crate) fn parse(text: &str) -> Result<Parsed, TqlError> {
    let tokens = lex(text)?;
    let mut parser = Parser {
        tokens,
        at: 0,
        depth: 0,
        atoms: 0,
        captures: Vec::new(),
        references: Vec::new(),
        regex_instructions: 0,
    };
    if parser.peek() == &Tok::Eof {
        return error(
            1,
            "the query is empty; start with a step such as `tool(bash)`",
        );
    }
    let mut node = parser.alternation()?;
    let mut within = None;
    if parser.peek_keyword("within") {
        let col = parser.col();
        parser.advance();
        within = Some(parser.within(col)?);
    }
    if parser.peek() != &Tok::Eof {
        let found = parser.peek().describe();
        let col = parser.col();
        let hint = if matches!(parser.peek(), Tok::Ident(_) | Tok::LParen) {
            "; join steps with `->` (next step) or `..>` (a later step)"
        } else {
            ""
        };
        return error(
            col,
            format!("unexpected {found}; expected `->`, `..>`, `|`, `within` or the end{hint}"),
        );
    }
    if step_bounds(&node).0 == 0 {
        return error(
            1,
            "this pattern can match zero steps (every part of it is optional), so it would \
             match every run; make at least one step required",
        );
    }
    // Binding positions, and captures that nothing binds.
    let mut bound = vec![false; parser.captures.len()];
    mark_bindings(&mut node, &mut bound);
    for (slot, col) in &parser.references {
        if !bound[*slot] {
            return error(
                *col,
                format!(
                    "capture `@{}` is never bound; bind it with `field=@{}` in a step's \
                     predicate (outside `or`/`not` and gaps) or as a name, like `tool(@{})`",
                    parser.captures[*slot], parser.captures[*slot], parser.captures[*slot]
                ),
            );
        }
    }
    Ok(Parsed {
        node,
        within,
        captures: parser.captures,
    })
}

/// Marks `field = @x` conditions in a predicate's top-level `and` chain, and
/// a lone capture in an atom's names, as binding positions.
fn mark_bindings(node: &mut Node, bound: &mut [bool]) {
    fn pred(pred: &mut Pred, positive: bool, bound: &mut [bool]) {
        match pred {
            Pred::And(items) => items
                .iter_mut()
                .for_each(|item| mark_pred(item, positive, bound)),
            Pred::Or(items) => items
                .iter_mut()
                .for_each(|item| mark_pred(item, false, bound)),
            Pred::Not(inner) => mark_pred(inner, false, bound),
            Pred::Cond(cond) => {
                if let Test::Compare(Op::Eq, Literal::Capture { slot, factor: None }) = cond.test {
                    if positive {
                        cond.test = Test::Bind(slot);
                        bound[slot] = true;
                    }
                }
            }
        }
    }
    fn mark_pred(item: &mut Pred, positive: bool, bound: &mut [bool]) {
        pred(item, positive, bound)
    }
    match node {
        Node::Atom(atom) => {
            if let [NameTest::Capture { slot, bind }] = atom.names.as_mut_slice() {
                *bind = true;
                bound[*slot] = true;
            }
            for item in &mut atom.preds {
                pred(item, true, bound);
            }
        }
        // Gap predicates test and never bind: a gap step is not part of the
        // match, and binding on it would capture from steps the match skips.
        Node::Gap(_) | Node::Start | Node::End => {}
        Node::Concat(items) | Node::Alternate(items) => {
            items.iter_mut().for_each(|item| mark_bindings(item, bound))
        }
        Node::Repeat { node, .. } => mark_bindings(node, bound),
    }
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.at].tok
    }

    fn col(&self) -> usize {
        self.tokens[self.at].col
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.at].clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        token
    }

    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Tok::Ident(text) if text.eq_ignore_ascii_case(keyword))
    }

    fn expect(&mut self, tok: Tok, context: &str) -> Result<Token, TqlError> {
        if self.peek() == &tok {
            return Ok(self.advance());
        }
        error(
            self.col(),
            format!(
                "expected `{}` {context}, found {}",
                tok.symbol(),
                self.peek().describe()
            ),
        )
    }

    fn enter(&mut self) -> Result<(), TqlError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return error(
                self.col(),
                format!("nesting deeper than {MAX_DEPTH} levels"),
            );
        }
        Ok(())
    }

    /// Compiles a regex written at `col`, counting it against the query's
    /// [`MAX_QUERY_REGEX_INSTRUCTIONS`].
    fn regex(&mut self, source: &str, insensitive: bool, col: usize) -> Result<Regex, TqlError> {
        let regex = compile_regex(source, insensitive, col)?;
        self.regex_instructions += regex.size();
        if self.regex_instructions > MAX_QUERY_REGEX_INSTRUCTIONS {
            return error(
                col,
                format!(
                    "the query's regexes compile to more than {MAX_QUERY_REGEX_INSTRUCTIONS} \
                     instructions in all; shorten them or lower their repetition counts"
                ),
            );
        }
        Ok(regex)
    }

    fn capture_slot(&mut self, name: &str, col: usize) -> Result<usize, TqlError> {
        let slot = match self.captures.iter().position(|known| known == name) {
            Some(slot) => slot,
            None => {
                if self.captures.len() == MAX_CAPTURES {
                    return error(col, format!("more than {MAX_CAPTURES} captures"));
                }
                self.captures.push(name.to_owned());
                self.captures.len() - 1
            }
        };
        self.references.push((slot, col));
        Ok(slot)
    }

    // ------------------------------------------------------------ patterns

    fn alternation(&mut self) -> Result<Node, TqlError> {
        self.enter()?;
        let mut branches = vec![self.sequence()?];
        while self.peek() == &Tok::Pipe {
            self.advance();
            branches.push(self.sequence()?);
        }
        self.depth -= 1;
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Start)
        } else {
            Node::Alternate(branches)
        })
    }

    fn sequence(&mut self) -> Result<Node, TqlError> {
        let mut items = Vec::new();
        if self.peek() == &Tok::Caret {
            self.advance();
            items.push(Node::Start);
        }
        items.push(self.term()?);
        loop {
            match self.peek() {
                Tok::Arrow => {
                    self.advance();
                    items.push(self.term()?);
                }
                Tok::DotDot => {
                    let gap = self.gap()?;
                    items.push(Node::Gap(Box::new(gap)));
                    items.push(self.term()?);
                }
                // `A (..> B){4,}`: a group that starts with a link continues
                // the step before it.
                Tok::LParen if self.continues() => {
                    let start = self.col();
                    let group = self.continuation()?;
                    items.push(self.quantified(group, start)?);
                }
                _ => break,
            }
        }
        if self.peek() == &Tok::Dollar {
            self.advance();
            items.push(Node::End);
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap_or(Node::Start)
        } else {
            Node::Concat(items)
        })
    }

    /// Whether the `(` at the cursor opens a continuation group: its first
    /// token is a link.
    fn continues(&self) -> bool {
        self.peek() == &Tok::LParen
            && matches!(
                self.tokens.get(self.at + 1).map(|token| &token.tok),
                Some(Tok::Arrow | Tok::DotDot)
            )
    }

    /// `( link term … | link term … )`.
    fn continuation(&mut self) -> Result<Node, TqlError> {
        let open = self.col();
        self.advance(); // `(`
        self.enter()?;
        let mut branches = vec![self.continued_sequence(open)?];
        while self.peek() == &Tok::Pipe {
            self.advance();
            branches.push(self.continued_sequence(open)?);
        }
        self.expect(
            Tok::RParen,
            &format!("to close the group opened at column {open}"),
        )?;
        self.depth -= 1;
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Start)
        } else {
            Node::Alternate(branches)
        })
    }

    fn continued_sequence(&mut self, open: usize) -> Result<Node, TqlError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Tok::Arrow => {
                    self.advance();
                    items.push(self.term()?);
                }
                Tok::DotDot => {
                    let gap = self.gap()?;
                    items.push(Node::Gap(Box::new(gap)));
                    items.push(self.term()?);
                }
                _ => break,
            }
        }
        if items.is_empty() {
            return error(
                self.col(),
                format!(
                    "the group opened at column {open} continues the step before it, so each of \
                     its alternatives starts with `->` or `..>`"
                ),
            );
        }
        if self.peek() == &Tok::Dollar {
            self.advance();
            items.push(Node::End);
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap_or(Node::Start)
        } else {
            Node::Concat(items)
        })
    }

    fn gap(&mut self) -> Result<Gap, TqlError> {
        let open = self.col();
        self.advance(); // `..`
        let mut gap = Gap {
            pred: None,
            min: 0,
            max: None,
        };
        if self.peek() == &Tok::LBracket {
            self.advance();
            gap.pred = Some(self.predicate()?);
            self.expect(Tok::RBracket, "to close the gap's predicate")?;
        }
        if self.peek() == &Tok::LBrace {
            let (min, max) = self.bounds(true)?;
            gap.min = min;
            gap.max = max;
        }
        if self.peek() != &Tok::Gt {
            return error(
                self.col(),
                format!(
                    "expected `>` to finish the `..` link opened at column {open} (write `..>`), \
                     found {}",
                    self.peek().describe()
                ),
            );
        }
        self.advance();
        Ok(gap)
    }

    /// `{n}`, `{n,}`, `{n,m}`, and for a gap also `{,m}`.
    fn bounds(&mut self, gap: bool) -> Result<(u32, Option<u32>), TqlError> {
        let open = self.col();
        self.advance(); // `{`
        let count = |parser: &mut Parser| -> Result<Option<u32>, TqlError> {
            match parser.peek().clone() {
                Tok::Num(value, true) if value >= 0.0 => {
                    let col = parser.col();
                    parser.advance();
                    if value > f64::from(MAX_COUNT) {
                        return error(col, format!("counts are capped at {MAX_COUNT}"));
                    }
                    Ok(Some(value as u32))
                }
                Tok::Num(..) => error(parser.col(), "a count is a whole number"),
                _ => Ok(None),
            }
        };
        let min = count(self)?;
        let (min, max) = if self.peek() == &Tok::Comma {
            self.advance();
            let max = count(self)?;
            match min {
                Some(min) => (min, max),
                None if gap => (0, max),
                None => return error(open, "write a repetition's minimum, like `{0,3}`"),
            }
        } else {
            match min {
                Some(min) => (min, Some(min)),
                None => return error(open, "expected a count, like `{3}` or `{2,5}`"),
            }
        };
        self.expect(Tok::RBrace, "to close the count")?;
        if max.is_some_and(|max| max < min) {
            return error(open, "the maximum is below the minimum");
        }
        Ok((min, max))
    }

    fn term(&mut self) -> Result<Node, TqlError> {
        let start = self.col();
        let node = self.primary()?;
        self.quantified(node, start)
    }

    /// `node` with the quantifier at the cursor applied, if there is one.
    fn quantified(&mut self, node: Node, start: usize) -> Result<Node, TqlError> {
        let col = self.col();
        let (min, max) = match self.peek() {
            Tok::Star => {
                self.advance();
                (0, None)
            }
            Tok::Plus => {
                self.advance();
                (1, None)
            }
            Tok::Question => {
                self.advance();
                (0, Some(1))
            }
            Tok::LBrace => self.bounds(false)?,
            _ => return Ok(node),
        };
        let greedy = if self.peek() == &Tok::Question {
            self.advance();
            false
        } else {
            true
        };
        if matches!(
            self.peek(),
            Tok::Star | Tok::Plus | Tok::Question | Tok::LBrace
        ) {
            return error(
                self.col(),
                "a quantifier cannot follow another; group the first, like `(tool+){2}`",
            );
        }
        if max == Some(0) {
            return error(col, "repeating zero times matches nothing; remove the step");
        }
        if step_bounds(&node).0 == 0 {
            return error(
                col,
                format!(
                    "the repeated part (from column {start}) can match zero steps, so repeating \
                     it is ambiguous; make it require a step"
                ),
            );
        }
        Ok(Node::Repeat {
            node: Box::new(node),
            min,
            max,
            greedy,
        })
    }

    fn primary(&mut self) -> Result<Node, TqlError> {
        let col = self.col();
        match self.peek().clone() {
            Tok::LParen if self.continues() => error(
                col,
                "a group that starts with `->` or `..>` continues the step before it; put a \
                 step first, like `tool(a) (..> tool(b)){2}`",
            ),
            Tok::LParen => {
                self.advance();
                let inner = self.alternation()?;
                self.expect(
                    Tok::RParen,
                    &format!("to close the group opened at column {col}"),
                )?;
                Ok(inner)
            }
            Tok::Ident(word) => {
                let lower = word.to_ascii_lowercase();
                let Some(kind) = AtomKind::ALL
                    .iter()
                    .copied()
                    .find(|kind| kind.keyword() == lower)
                else {
                    let hint =
                        ast::suggest(&lower, AtomKind::ALL.iter().map(|kind| kind.keyword()))
                            .map(|close| format!("; did you mean `{close}`?"))
                            .unwrap_or_default();
                    return error(
                        col,
                        format!(
                            "unknown step kind `{word}`{hint} Steps are tool, llm, agent, step, \
                             edit and test"
                        ),
                    );
                };
                self.advance();
                self.atom(kind, col)
            }
            Tok::Caret => error(col, "`^` may only start a sequence"),
            Tok::Dollar => error(col, "`$` may only end a sequence"),
            Tok::Eof => error(col, "the query ends where a step was expected"),
            other => error(
                col,
                format!(
                    "expected a step (tool, llm, agent, step, edit, test) or `(`, found {}",
                    other.describe()
                ),
            ),
        }
    }

    fn atom(&mut self, kind: AtomKind, col: usize) -> Result<Node, TqlError> {
        self.atoms += 1;
        if self.atoms > MAX_ATOMS {
            return error(col, format!("more than {MAX_ATOMS} steps in one query"));
        }
        let mut names = Vec::new();
        if self.peek() == &Tok::LParen && !self.continues() {
            if kind == AtomKind::Test {
                return error(
                    self.col(),
                    "`test` takes no name; filter it with a predicate, like `test[args~\"pytest\"]`",
                );
            }
            let open = self.col();
            self.advance();
            let mut any = false;
            loop {
                let name_col = self.col();
                match self.peek().clone() {
                    Tok::Ident(text) | Tok::Str(text) => {
                        names.push(NameTest::Exact(text));
                        self.advance();
                    }
                    Tok::Regex(source, insensitive) => {
                        names.push(NameTest::Regex(self.regex(
                            &source,
                            insensitive,
                            name_col + 1,
                        )?));
                        self.advance();
                    }
                    Tok::Tilde => {
                        self.advance();
                        let regex_col = self.col();
                        match self.peek().clone() {
                            Tok::Str(source) => {
                                names.push(NameTest::Regex(self.regex(
                                    &source,
                                    false,
                                    regex_col + 1,
                                )?));
                                self.advance();
                            }
                            other => {
                                return error(
                                    regex_col,
                                    format!(
                                        "expected a quoted regex after `~`, found {}",
                                        other.describe()
                                    ),
                                )
                            }
                        }
                    }
                    Tok::Capture(name) => {
                        let slot = self.capture_slot(&name, name_col)?;
                        names.push(NameTest::Capture { slot, bind: false });
                        self.advance();
                    }
                    Tok::Star => {
                        any = true;
                        self.advance();
                    }
                    other => {
                        return error(
                            name_col,
                            format!(
                                "expected a name, \"quoted name\", /regex/, @capture or `*`, \
                                 found {}",
                                other.describe()
                            ),
                        )
                    }
                }
                if self.peek() == &Tok::Pipe {
                    self.advance();
                    continue;
                }
                self.expect(
                    Tok::RParen,
                    &format!("to close the names opened at column {open}"),
                )?;
                break;
            }
            if any {
                names.clear();
            }
        }
        let mut preds = Vec::new();
        while self.peek() == &Tok::LBracket {
            self.advance();
            preds.push(self.predicate()?);
            self.expect(Tok::RBracket, "to close the predicate")?;
        }
        Ok(Node::Atom(Box::new(Atom { kind, names, preds })))
    }

    // ---------------------------------------------------------- predicates

    fn predicate(&mut self) -> Result<Pred, TqlError> {
        self.enter()?;
        let mut items = vec![self.conjunction()?];
        while self.peek_keyword("or") {
            self.advance();
            items.push(self.conjunction()?);
        }
        self.depth -= 1;
        Ok(if items.len() == 1 {
            items.pop().unwrap_or(Pred::And(Vec::new()))
        } else {
            Pred::Or(items)
        })
    }

    fn conjunction(&mut self) -> Result<Pred, TqlError> {
        let mut items = vec![self.negation()?];
        while self.peek_keyword("and") || self.peek() == &Tok::Comma {
            self.advance();
            items.push(self.negation()?);
        }
        Ok(if items.len() == 1 {
            items.pop().unwrap_or(Pred::And(Vec::new()))
        } else {
            Pred::And(items)
        })
    }

    fn negation(&mut self) -> Result<Pred, TqlError> {
        if self.peek_keyword("not") {
            self.advance();
            self.enter()?;
            let inner = self.negation()?;
            self.depth -= 1;
            return Ok(Pred::Not(Box::new(inner)));
        }
        if self.peek() == &Tok::LParen {
            let open = self.col();
            self.advance();
            let inner = self.predicate()?;
            self.expect(Tok::RParen, &format!("to close the `(` at column {open}"))?;
            return Ok(inner);
        }
        self.condition()
    }

    fn condition(&mut self) -> Result<Pred, TqlError> {
        let col = self.col();
        let (field, ty) = self.field()?;
        let op = match self.peek() {
            Tok::Eq => Op::Eq,
            Tok::Ne => Op::Ne,
            Tok::Tilde => Op::Match,
            Tok::NotTilde => Op::NotMatch,
            Tok::Gt => Op::Gt,
            Tok::Ge => Op::Ge,
            Tok::Lt => Op::Lt,
            Tok::Le => Op::Le,
            _ => {
                return Ok(Pred::Cond(Cond {
                    field,
                    test: Test::Truthy,
                }))
            }
        };
        let op_col = self.col();
        self.advance();
        let literal = self.value(&field, ty, op, op_col, col)?;
        Ok(Pred::Cond(Cond {
            field,
            test: Test::Compare(op, literal),
        }))
    }

    fn field(&mut self) -> Result<(Field, FieldType), TqlError> {
        let col = self.col();
        let Tok::Ident(text) = self.peek().clone() else {
            return error(
                col,
                format!(
                    "expected a field such as `status` or `args`, found {}",
                    self.peek().describe()
                ),
            );
        };
        self.advance();
        let (head, rest) = match text.split_once('.') {
            Some((head, rest)) => (head.to_owned(), Some(rest.to_owned())),
            None => (text.clone(), None),
        };
        let path_tail = |parser: &mut Parser, rest: String| -> Result<Vec<String>, TqlError> {
            let mut segments: Vec<String> = Vec::new();
            let trailing_dot = rest.ends_with('.') || rest.is_empty();
            for segment in rest.split('.').filter(|segment| !segment.is_empty()) {
                segments.push(segment.to_owned());
            }
            if trailing_dot {
                match parser.peek().clone() {
                    Tok::Str(segment) => {
                        parser.advance();
                        segments.push(segment);
                    }
                    _ => {
                        return error(
                            col,
                            format!("`{text}` ends with `.`; name the key after it"),
                        )
                    }
                }
            }
            Ok(segments)
        };
        match (head.as_str(), rest) {
            ("attr", Some(rest)) => {
                let segments = path_tail(self, rest)?;
                Ok((Field::Attr(segments.join(".")), FieldType::Dynamic))
            }
            ("args" | "input", Some(rest)) => {
                let segments = path_tail(self, rest)?;
                Ok((Field::ArgsPath(segments), FieldType::Dynamic))
            }
            ("result" | "output", Some(rest)) => {
                let segments = path_tail(self, rest)?;
                Ok((Field::ResultPath(segments), FieldType::Dynamic))
            }
            ("attr", None) => error(col, "`attr` needs a key, like `attr.http.status_code`"),
            (_, Some(_)) => error(
                col,
                format!("`{head}` has no sub-fields; only args., result. and attr. take a path"),
            ),
            (name, None) => Field::named(name).ok_or_else(|| {
                let hint = ast::suggest(name, ast::FIELDS.iter().map(|(name, ..)| *name))
                    .map(|close| format!("; did you mean `{close}`?"))
                    .unwrap_or_default();
                TqlError {
                    message: format!(
                        "unknown field `{name}`{hint} Fields: {}, plus args.<path>, \
                         result.<path> and attr.<key>",
                        ast::FIELDS
                            .iter()
                            .map(|(name, ..)| *name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    column: col,
                }
            }),
        }
    }

    fn value(
        &mut self,
        field: &Field,
        ty: FieldType,
        op: Op,
        op_col: usize,
        field_col: usize,
    ) -> Result<Literal, TqlError> {
        let col = self.col();
        let name = field.render();
        let token = self.peek().clone();
        let literal = match token {
            Tok::Str(text) => {
                self.advance();
                if matches!(op, Op::Match | Op::NotMatch) {
                    Literal::Regex(self.regex(&text, false, col + 1)?)
                } else {
                    Literal::Str(text)
                }
            }
            Tok::Regex(source, insensitive) => {
                self.advance();
                if !matches!(op, Op::Match | Op::NotMatch) {
                    return error(
                        col,
                        format!("a /regex/ goes with `~` or `!~`, not `{}`", op.symbol()),
                    );
                }
                Literal::Regex(self.regex(&source, insensitive, col + 1)?)
            }
            Tok::Ident(word) => {
                self.advance();
                if word.eq_ignore_ascii_case("true") {
                    Literal::Bool(true)
                } else if word.eq_ignore_ascii_case("false") {
                    Literal::Bool(false)
                } else if matches!(op, Op::Match | Op::NotMatch) {
                    Literal::Regex(self.regex(&word, false, col)?)
                } else {
                    Literal::Str(word)
                }
            }
            Tok::Num(value, _) => {
                self.advance();
                if self.peek() == &Tok::Star {
                    self.advance();
                    let capture_col = self.col();
                    match self.peek().clone() {
                        Tok::Capture(capture) => {
                            self.advance();
                            let slot = self.capture_slot(&capture, capture_col)?;
                            Literal::Capture {
                                slot,
                                factor: Some(value),
                            }
                        }
                        other => {
                            return error(
                                capture_col,
                                format!("expected a capture after `*`, found {}", other.describe()),
                            )
                        }
                    }
                } else {
                    Literal::Num(value)
                }
            }
            Tok::Duration(nanos) => {
                self.advance();
                Literal::Duration(nanos)
            }
            Tok::Capture(capture) => {
                self.advance();
                let slot = self.capture_slot(&capture, col)?;
                let factor = if self.peek() == &Tok::Star {
                    self.advance();
                    match self.peek().clone() {
                        Tok::Num(value, _) => {
                            self.advance();
                            Some(value)
                        }
                        other => {
                            return error(
                                self.col(),
                                format!("expected a number after `*`, found {}", other.describe()),
                            )
                        }
                    }
                } else {
                    None
                };
                Literal::Capture { slot, factor }
            }
            other => {
                return error(
                    col,
                    format!(
                        "expected a value after `{}`, found {}",
                        op.symbol(),
                        other.describe()
                    ),
                )
            }
        };
        check_types(&name, ty, op, &literal, op_col, col, field_col)?;
        Ok(literal)
    }

    fn within(&mut self, col: usize) -> Result<Within, TqlError> {
        let value_col = self.col();
        let within = match self.peek().clone() {
            Tok::Num(value, true) if value >= 1.0 => {
                self.advance();
                if self.peek_keyword("steps") || self.peek_keyword("step") {
                    self.advance();
                }
                if value > 100_000.0 {
                    return error(value_col, "`within` counts at most 100000 steps");
                }
                Within::Steps(value as u64)
            }
            Tok::Duration(nanos) if nanos > 0 => {
                self.advance();
                Within::Duration(nanos)
            }
            _ => {
                return error(
                    value_col,
                    format!(
                        "`within` at column {col} takes a step count (`within 10`) or a duration \
                         (`within 5m`)"
                    ),
                )
            }
        };
        Ok(within)
    }
}

/// Compiles a regex literal, mapping its refusal to a column in the query.
/// `col` is the column of the regex's first character.
fn compile_regex(source: &str, insensitive: bool, col: usize) -> Result<Regex, TqlError> {
    let full = if insensitive && !source.starts_with("(?i)") {
        format!("(?i){source}")
    } else {
        source.to_owned()
    };
    Regex::new(&full).map_err(|refusal| {
        let shift = if insensitive && !source.starts_with("(?i)") {
            4
        } else {
            0
        };
        TqlError {
            message: format!("in regex: {}", refusal.message),
            column: col + refusal.offset.saturating_sub(shift),
        }
    })
}

fn check_types(
    name: &str,
    ty: FieldType,
    op: Op,
    literal: &Literal,
    op_col: usize,
    col: usize,
    field_col: usize,
) -> Result<(), TqlError> {
    let ordering = matches!(op, Op::Gt | Op::Ge | Op::Lt | Op::Le);
    let matching = matches!(op, Op::Match | Op::NotMatch);
    match ty {
        FieldType::Number | FieldType::Duration if matching => {
            return error(op_col, format!("`{name}` is a number; `~` matches text"))
        }
        FieldType::Bool if matching || ordering => {
            return error(
                op_col,
                format!("`{name}` is true or false: test it bare (`[{name}]`), with `not`, or with `=true`/`=false`"),
            )
        }
        FieldType::Text | FieldType::List if ordering => {
            return error(op_col, format!("`{name}` is text; compare it with `=`, `!=` or `~`"))
        }
        _ => {}
    }
    match (ty, literal) {
        (FieldType::Duration, Literal::Num(_)) => {
            error(col, format!("give `{name}` a unit, like `30s` or `500ms`"))
        }
        (FieldType::Duration, Literal::Duration(_) | Literal::Capture { .. }) => Ok(()),
        (FieldType::Duration, _) => error(
            col,
            format!("`{name}` compares with a duration, like `30s`"),
        ),
        (_, Literal::Duration(_)) => error(
            col,
            format!("a duration compares with `duration`, not `{name}` (column {field_col})"),
        ),
        (FieldType::Number, Literal::Str(_) | Literal::Bool(_)) => error(
            col,
            format!("`{name}` is a number; compare it with a number"),
        ),
        (FieldType::Bool, Literal::Str(_) | Literal::Num(_)) => {
            error(col, format!("`{name}` is true or false"))
        }
        (FieldType::Dynamic, Literal::Str(_) | Literal::Bool(_)) if ordering => {
            error(col, format!("`{name}` is ordered against numbers only"))
        }
        (
            _,
            Literal::Capture {
                factor: Some(_), ..
            },
        ) if matching => error(col, "a scaled capture is a number; `~` matches text"),
        (_, Literal::Capture { .. }) if matching => error(
            col,
            "`~` takes a regex, not a capture; compare captured values with `=`",
        ),
        _ => Ok(()),
    }
}
