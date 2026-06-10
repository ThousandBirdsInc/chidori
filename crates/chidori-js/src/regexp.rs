//! A self-contained, pure-Rust backtracking regular-expression engine plus the
//! `Vm::make_regexp` builder that the bytecode compiler calls for regexp
//! literals.
//!
//! The engine works over `&[char]` (Unicode scalar values), matching the rest of
//! the engine's code-point string model. It supports the common JS subset:
//!
//!   * literals and escaped metacharacters
//!   * the dot `.` (any char except line terminators, unless the `s` flag is set)
//!   * character classes `[...]` / `[^...]` with ranges and the class escapes
//!     `\d \D \w \W \s \S`
//!   * the escapes `\n \t \r \f \v \0`, control escapes `\cX`, hex `\xHH`,
//!     unicode `\uHHHH` / `\u{...}`, octal escapes, identity escapes, and the
//!     `\b \B` word-boundary assertions
//!   * anchors `^` and `$` (line-aware under the `m` flag)
//!   * capturing groups `(...)`, non-capturing groups `(?:...)`, named capture
//!     groups `(?<name>...)`
//!   * lookahead `(?=...)` / `(?!...)` and lookbehind `(?<=...)` / `(?<!...)`
//!     (the latter implemented by trying to match the sub-pattern that *ends* at
//!     the current position — works for both fixed and variable width)
//!   * alternation `|`
//!   * quantifiers `*` `+` `?` `{n}` `{n,}` `{n,m}` and their lazy `?` variants
//!   * backreferences `\1` .. `\99` and named backreferences `\k<name>`
//!   * flags `g i m s y` (`u`/`d`/`v` are accepted; `u` is largely a no-op)
//!
//! Unsupported constructs (currently Unicode property escapes `\p{...}`) cause
//! the parser to return an error, which surfaces as a SyntaxError at
//! `make_regexp` time rather than a panic. The matcher never indexes out of
//! bounds and bounds catastrophic backtracking with a shared step budget.

use crate::value::*;
use crate::vm::Vm;

// =========================================================================
// Public API
// =========================================================================

/// The result of a successful match: char-index span of the whole match
/// (`group 0`) plus each capture group's span (`None` when the group did not
/// participate).
#[derive(Clone, Debug)]
pub struct ReMatch {
    pub start: usize,
    pub end: usize,
    /// `groups[0]` is the whole match; `groups[i]` is capture group `i`.
    pub groups: Vec<Option<(usize, usize)>>,
}

/// Compile `pattern`/`flags` and search `input` for a match at or after `start`
/// (unless the `y` sticky flag forces a match exactly at `start`). Returns
/// `None` if the pattern is unsupported or no match is found.
pub fn regex_exec(pattern: &str, flags: &str, input: &[char], start: usize) -> Option<ReMatch> {
    let re = Regex::compile(pattern, flags).ok()?;
    re.exec(input, start)
}

/// Like [`regex_exec`], but additionally returns the `name -> group index`
/// mapping declared by any named capture groups `(?<name>...)` in the pattern
/// (in declaration order). Callers (e.g. `RegExp.prototype.exec`) use this to
/// populate the `groups` object on a match result. The returned `ReMatch` is
/// identical to what `regex_exec` would produce; the names list is empty when
/// the pattern declares no named groups.
pub fn regex_exec_named(
    pattern: &str,
    flags: &str,
    input: &[char],
    start: usize,
) -> Option<(ReMatch, Vec<(String, usize)>)> {
    let re = Regex::compile(pattern, flags).ok()?;
    let names = re.group_names.clone();
    re.exec(input, start).map(|m| (m, names))
}

/// Returns the `name -> group index` mapping for any named capture groups in
/// `pattern`/`flags`, or an empty list if there are none (or the pattern fails
/// to compile). Lets callers attach a `groups` object even on a `null` exec
/// where the match itself failed.
pub fn regexp_group_names(pattern: &str, flags: &str) -> Vec<(String, usize)> {
    match Regex::compile(pattern, flags) {
        Ok(re) => re.group_names,
        Err(_) => Vec::new(),
    }
}

/// Returns true if `pattern`/`flags` parse with the supported subset. Used to
/// fail loudly (SyntaxError) on unsupported constructs.
pub fn regex_is_valid(pattern: &str, flags: &str) -> Result<(), String> {
    Regex::compile(pattern, flags).map(|_| ())
}

// =========================================================================
// Flags
// =========================================================================

#[derive(Clone, Copy, Default)]
struct Flags {
    global: bool,
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    sticky: bool,
    /// `u` or `v` (Unicode mode): enables property escapes `\p{...}` and makes
    /// otherwise-invalid escapes a SyntaxError rather than an identity escape.
    unicode: bool,
}

impl Flags {
    fn parse(flags: &str) -> Result<Flags, String> {
        let mut f = Flags::default();
        let mut seen = [false; 128];
        for c in flags.chars() {
            // A repeated flag is a SyntaxError.
            let idx = c as usize;
            if idx < 128 {
                if seen[idx] {
                    return Err(format!("Duplicate regular expression flag '{c}'"));
                }
                seen[idx] = true;
            }
            match c {
                'g' => f.global = true,
                'i' => f.ignore_case = true,
                'm' => f.multiline = true,
                's' => f.dot_all = true,
                'y' => f.sticky = true,
                'u' | 'v' => f.unicode = true,
                'd' => {} // hasIndices: accepted, no matcher effect here
                other => return Err(format!("Invalid regular expression flag '{other}'")),
            }
        }
        Ok(f)
    }
}

// =========================================================================
// AST
// =========================================================================

#[derive(Clone, Debug)]
enum Node {
    /// Matches the empty string.
    Empty,
    /// A single literal character.
    Char(char),
    /// `.` — any char (line terminators excluded unless dotAll).
    AnyChar,
    /// A character class.
    Class { negated: bool, items: Vec<ClassItem> },
    /// `^`
    Start,
    /// `$`
    End,
    /// `\b` (true) / `\B` (false).
    WordBoundary(bool),
    /// A group; `Some(idx)` for capturing groups, `None` for `(?:...)`.
    Group { idx: Option<usize>, node: Box<Node> },
    /// A lookaround assertion. `behind` selects lookbehind vs lookahead;
    /// `negative` selects the `!` form.
    Look {
        behind: bool,
        negative: bool,
        node: Box<Node>,
    },
    /// Alternation `a|b|c`.
    Alt(Vec<Node>),
    /// Sequence of nodes.
    Concat(Vec<Node>),
    /// Quantified subpattern.
    Repeat {
        node: Box<Node>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    /// Backreference `\1` .. `\99` or named `\k<name>` (resolved to an index).
    Backref(usize),
}

#[derive(Clone, Debug)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
    /// A Unicode property escape `\p{...}` / `\P{...}`, resolved to code-point
    /// ranges at parse time. `negated` is true for `\P`.
    Unicode {
        ranges: std::rc::Rc<Vec<(char, char)>>,
        negated: bool,
    },
}

// =========================================================================
// Parser
// =========================================================================

struct Parser<'a> {
    src: &'a [char],
    pos: usize,
    group_count: usize,
    /// `(name, group_index)` for each named capture group, in declaration order.
    group_names: Vec<(String, usize)>,
    /// Named backreferences `\k<name>` whose target may appear later in the
    /// pattern; resolved after the full parse.
    pending_named_backrefs: Vec<(String, usize)>, // (name, AST Backref placeholder slot id)
    /// Backing store for the placeholder backref indices recorded above; index
    /// into this vec is the placeholder id, value is the resolved group index
    /// (0 until resolved, which makes the backref match the empty string).
    named_backref_targets: Vec<usize>,
    /// Whether the `u`/`v` flag is set (affects `\p`, strict escapes).
    unicode: bool,
}

impl<'a> Parser<'a> {
    fn new(src: &'a [char], unicode: bool) -> Parser<'a> {
        Parser {
            src,
            pos: 0,
            group_count: 0,
            group_names: Vec::new(),
            pending_named_backrefs: Vec::new(),
            named_backref_targets: Vec::new(),
            unicode,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }
    fn next(&mut self) -> Option<char> {
        let c = self.src.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Top level: an alternation.
    fn parse(&mut self) -> Result<Node, String> {
        let mut node = self.parse_alternation()?;
        if self.pos != self.src.len() {
            return Err(format!(
                "Unexpected token in regular expression at position {}",
                self.pos
            ));
        }
        // Resolve any named backreferences now that all group names are known.
        // Collect into a local first so we don't hold overlapping field borrows.
        let pending: Vec<(String, usize)> = std::mem::take(&mut self.pending_named_backrefs);
        for (name, slot) in &pending {
            let target = self
                .group_names
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, idx)| *idx);
            match target {
                Some(idx) => self.named_backref_targets[*slot] = idx,
                None => {
                    return Err(format!("Invalid named capture referenced: '{name}'"))
                }
            }
        }
        // Rewrite placeholder backrefs (encoded as Backref(usize::MAX - slot))
        // with their resolved group indices.
        if !pending.is_empty() {
            resolve_named_backrefs(&mut node, &self.named_backref_targets);
        }
        // In unicode mode a numeric backreference to a group that does not exist
        // is a SyntaxError (in non-unicode mode it is a legacy octal/identity
        // escape, handled at parse time).
        if self.unicode && has_out_of_range_backref(&node, self.group_count) {
            return Err("Invalid backreference to a non-existent group".to_string());
        }
        Ok(node)
    }

    fn parse_alternation(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.eat('|') {
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Node::Alt(branches))
        }
    }

    fn parse_concat(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {}
            }
            let atom = self.parse_quantified()?;
            items.push(atom);
        }
        if items.is_empty() {
            Ok(Node::Empty)
        } else if items.len() == 1 {
            Ok(items.pop().unwrap())
        } else {
            Ok(Node::Concat(items))
        }
    }

    fn parse_quantified(&mut self) -> Result<Node, String> {
        let atom = self.parse_atom()?;
        // Assertions (anchors, boundaries, lookaround) cannot be quantified in a
        // meaningful way; per spec lookarounds *can* take a quantifier in Annex
        // B, but quantifying a pure assertion is harmless here, so we allow it
        // uniformly except we never reach this for `^`/`$` differently.
        let (min, max) = match self.peek() {
            Some('*') => {
                self.pos += 1;
                (0, None)
            }
            Some('+') => {
                self.pos += 1;
                (1, None)
            }
            Some('?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some('{') => {
                // Try to parse a {n}/{n,}/{n,m} quantifier; if it doesn't parse
                // as one, treat `{` as a literal in non-unicode mode. In unicode
                // mode a lone `{` is a SyntaxError.
                match self.try_parse_brace_quantifier()? {
                    Some(mm) => mm,
                    None => {
                        if self.unicode {
                            return Err("Incomplete quantifier".to_string());
                        }
                        return Ok(atom);
                    }
                }
            }
            _ => return Ok(atom),
        };
        // In unicode mode, anchors/boundaries/lookaround cannot be quantified.
        if self.unicode && is_assertion(&atom) {
            return Err("Invalid quantifier on assertion".to_string());
        }
        let greedy = !self.eat('?');
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    /// Parse `{n}`, `{n,}`, `{n,m}` starting at a `{`. Returns `None` (without
    /// consuming) if the braces don't form a valid quantifier. Returns `Err`
    /// when the bounds are present but out of order (`{2,1}`).
    fn try_parse_brace_quantifier(&mut self) -> Result<Option<(usize, Option<usize>)>, String> {
        let save = self.pos;
        // consume '{'
        self.pos += 1;
        let min = match self.parse_decimal() {
            Some(n) => n,
            None => {
                self.pos = save;
                return Ok(None);
            }
        };
        let max;
        if self.eat(',') {
            if self.peek() == Some('}') {
                max = None;
            } else {
                match self.parse_decimal() {
                    Some(n) => max = Some(n),
                    None => {
                        self.pos = save;
                        return Ok(None);
                    }
                }
            }
        } else {
            max = Some(min);
        }
        if !self.eat('}') {
            self.pos = save;
            return Ok(None);
        }
        if let Some(m) = max {
            if m < min {
                return Err("numbers out of order in {} quantifier".to_string());
            }
        }
        Ok(Some((min, max)))
    }

    fn parse_decimal(&mut self) -> Option<usize> {
        let start = self.pos;
        let mut n: usize = 0;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                n = n.saturating_mul(10).saturating_add((c as u8 - b'0') as usize);
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            None
        } else {
            Some(n)
        }
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.peek() {
            Some('(') => self.parse_group(),
            Some('[') => self.parse_class(),
            Some('.') => {
                self.pos += 1;
                Ok(Node::AnyChar)
            }
            Some('^') => {
                self.pos += 1;
                Ok(Node::Start)
            }
            Some('$') => {
                self.pos += 1;
                Ok(Node::End)
            }
            Some('\\') => self.parse_escape(),
            Some(c @ ('*' | '+' | '?')) => Err(format!("Nothing to repeat before '{c}'")),
            // In unicode mode, `{`, `}`, and `]` are only valid in their
            // structural roles; a bare one here is a SyntaxError (in non-unicode
            // mode they are accepted as literal PatternCharacters).
            Some(c @ ('}' | ']' | '{')) if self.unicode => {
                Err(format!("Lone '{c}' is not allowed in unicode mode"))
            }
            Some(c) => {
                self.pos += 1;
                Ok(Node::Char(c))
            }
            None => Ok(Node::Empty),
        }
    }

    fn parse_group(&mut self) -> Result<Node, String> {
        // consume '('
        self.pos += 1;
        let mut capturing = true;
        let mut lookaround: Option<(bool, bool)> = None; // (behind, negative)
        let mut group_name: Option<String> = None;

        if self.peek() == Some('?') {
            match self.src.get(self.pos + 1).copied() {
                Some(':') => {
                    self.pos += 2;
                    capturing = false;
                }
                Some('=') => {
                    self.pos += 2;
                    lookaround = Some((false, false));
                    capturing = false;
                }
                Some('!') => {
                    self.pos += 2;
                    lookaround = Some((false, true));
                    capturing = false;
                }
                Some('<') => {
                    // Could be lookbehind `(?<=`/`(?<!` or named group `(?<name>`.
                    match self.src.get(self.pos + 2).copied() {
                        Some('=') => {
                            self.pos += 3;
                            lookaround = Some((true, false));
                            capturing = false;
                        }
                        Some('!') => {
                            self.pos += 3;
                            lookaround = Some((true, true));
                            capturing = false;
                        }
                        _ => {
                            // Named capture group: `(?<name>...)`.
                            self.pos += 2; // consume `?<`
                            let name = self.parse_group_name()?;
                            group_name = Some(name);
                            // capturing stays true
                        }
                    }
                }
                _ => return Err("Invalid group".to_string()),
            }
        }

        if let Some((behind, negative)) = lookaround {
            let inner = self.parse_alternation()?;
            if !self.eat(')') {
                return Err("Unterminated group".to_string());
            }
            return Ok(Node::Look {
                behind,
                negative,
                node: Box::new(inner),
            });
        }

        let idx = if capturing {
            self.group_count += 1;
            let i = self.group_count;
            if let Some(name) = group_name {
                if self.group_names.iter().any(|(n, _)| *n == name) {
                    return Err(format!("Duplicate capture group name '{name}'"));
                }
                self.group_names.push((name, i));
            }
            Some(i)
        } else {
            None
        };
        let inner = self.parse_alternation()?;
        if !self.eat(')') {
            return Err("Unterminated group".to_string());
        }
        Ok(Node::Group {
            idx,
            node: Box::new(inner),
        })
    }

    /// Parse a group name `name>` (the leading `?<` already consumed). Names are
    /// JS identifier-ish; we accept identifier-start/-continue chars plus `$`
    /// and `_`, terminated by `>`.
    fn parse_group_name(&mut self) -> Result<String, String> {
        let mut name = String::new();
        loop {
            match self.next() {
                None => return Err("Unterminated group name".to_string()),
                Some('>') => break,
                Some(c) => {
                    let ok = if name.is_empty() {
                        is_id_start(c)
                    } else {
                        is_id_continue(c)
                    };
                    if !ok {
                        return Err(format!("Invalid character '{c}' in group name"));
                    }
                    name.push(c);
                }
            }
        }
        if name.is_empty() {
            return Err("Empty capture group name".to_string());
        }
        Ok(name)
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        // consume '['
        self.pos += 1;
        let negated = self.eat('^');
        let mut items: Vec<ClassItem> = Vec::new();
        loop {
            match self.peek() {
                None => return Err("Unterminated character class".to_string()),
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => {}
            }
            let lo = self.parse_class_atom()?;
            // Range?  `a-z` — but only when both ends are plain chars and a `-`
            // is followed by a non-`]` atom.
            if let ClassAtom::Char(lo_c) = lo {
                if self.peek() == Some('-') && self.src.get(self.pos + 1).copied() != Some(']') {
                    // consume '-'
                    self.pos += 1;
                    let hi = self.parse_class_atom()?;
                    match hi {
                        ClassAtom::Char(hi_c) => {
                            if (lo_c as u32) > (hi_c as u32) {
                                return Err("Range out of order in character class".to_string());
                            }
                            items.push(ClassItem::Range(lo_c, hi_c));
                            continue;
                        }
                        ClassAtom::Class(item) => {
                            // e.g. `[a-\d]` — treat '-' literally.
                            items.push(ClassItem::Char(lo_c));
                            items.push(ClassItem::Char('-'));
                            items.push(item);
                            continue;
                        }
                    }
                }
            }
            match lo {
                ClassAtom::Char(c) => items.push(ClassItem::Char(c)),
                ClassAtom::Class(item) => items.push(item),
            }
        }
        Ok(Node::Class { negated, items })
    }

    fn parse_class_atom(&mut self) -> Result<ClassAtom, String> {
        match self.next() {
            None => Err("Unterminated character class".to_string()),
            Some('\\') => self.parse_class_escape(),
            Some(c) => Ok(ClassAtom::Char(c)),
        }
    }

    /// Parse the `{...}` body of a `\p`/`\P` escape (the `p`/`P` is already
    /// consumed) and resolve it to code-point ranges. The `u`/`v` flag must be
    /// set (callers gate on `self.unicode`).
    fn parse_unicode_property(&mut self) -> Result<std::rc::Rc<Vec<(char, char)>>, String> {
        if self.peek() != Some('{') {
            return Err("Invalid property escape: expected '{' after \\p".to_string());
        }
        self.pos += 1; // consume '{'
        let mut name = String::new();
        loop {
            match self.peek() {
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                Some(ch) => {
                    name.push(ch);
                    self.pos += 1;
                }
                None => return Err("Unterminated \\p{...} property escape".to_string()),
            }
        }
        resolve_unicode_property(&name)
    }

    fn parse_class_escape(&mut self) -> Result<ClassAtom, String> {
        match self.peek() {
            None => Err("Trailing backslash in character class".to_string()),
            Some(c) => Ok(match c {
                'd' => {
                    self.pos += 1;
                    ClassAtom::Class(ClassItem::Digit)
                }
                'D' => {
                    self.pos += 1;
                    ClassAtom::Class(ClassItem::NotDigit)
                }
                'w' => {
                    self.pos += 1;
                    ClassAtom::Class(ClassItem::Word)
                }
                'W' => {
                    self.pos += 1;
                    ClassAtom::Class(ClassItem::NotWord)
                }
                's' => {
                    self.pos += 1;
                    ClassAtom::Class(ClassItem::Space)
                }
                'S' => {
                    self.pos += 1;
                    ClassAtom::Class(ClassItem::NotSpace)
                }
                'p' | 'P' => {
                    // `\p{...}`/`\P{...}` Unicode property escape inside a class.
                    self.pos += 1; // consume 'p'/'P'
                    if self.unicode {
                        let negated = c == 'P';
                        let ranges = self.parse_unicode_property()?;
                        ClassAtom::Class(ClassItem::Unicode { ranges, negated })
                    } else {
                        ClassAtom::Char(c)
                    }
                }
                'n' => {
                    self.pos += 1;
                    ClassAtom::Char('\n')
                }
                't' => {
                    self.pos += 1;
                    ClassAtom::Char('\t')
                }
                'r' => {
                    self.pos += 1;
                    ClassAtom::Char('\r')
                }
                'f' => {
                    self.pos += 1;
                    ClassAtom::Char('\u{000C}')
                }
                'v' => {
                    self.pos += 1;
                    ClassAtom::Char('\u{000B}')
                }
                'b' => {
                    self.pos += 1;
                    ClassAtom::Char('\u{0008}') // \b is backspace inside a class
                }
                'c' => {
                    // \cX control escape; if not a valid control letter, treat
                    // the backslash literally (Annex B leniency).
                    self.pos += 1;
                    match self.peek() {
                        Some(ch) if ch.is_ascii_alphabetic() => {
                            self.pos += 1;
                            ClassAtom::Char(((ch as u8) & 0x1f) as char)
                        }
                        _ => ClassAtom::Char('\\'),
                    }
                }
                'x' => {
                    self.pos += 1;
                    match self.parse_hex(2) {
                        Some(ch) => ClassAtom::Char(ch),
                        None => ClassAtom::Char('x'),
                    }
                }
                'u' => {
                    self.pos += 1;
                    match self.parse_unicode_escape() {
                        Some(ch) => ClassAtom::Char(ch),
                        None => ClassAtom::Char('u'),
                    }
                }
                '0'..='7' => {
                    // Octal escape inside a class (Annex B). `\0` not followed by
                    // a digit is NUL.
                    ClassAtom::Char(self.parse_octal_escape())
                }
                _ => {
                    let other = self.next().unwrap();
                    ClassAtom::Char(other)
                }
            }),
        }
    }

    fn parse_escape(&mut self) -> Result<Node, String> {
        // consume '\\'
        self.pos += 1;
        let c = match self.peek() {
            Some(c) => c,
            None => return Err("Trailing backslash in regular expression".to_string()),
        };
        Ok(match c {
            'd' => {
                self.pos += 1;
                Node::Class {
                    negated: false,
                    items: vec![ClassItem::Digit],
                }
            }
            'D' => {
                self.pos += 1;
                Node::Class {
                    negated: false,
                    items: vec![ClassItem::NotDigit],
                }
            }
            'w' => {
                self.pos += 1;
                Node::Class {
                    negated: false,
                    items: vec![ClassItem::Word],
                }
            }
            'W' => {
                self.pos += 1;
                Node::Class {
                    negated: false,
                    items: vec![ClassItem::NotWord],
                }
            }
            's' => {
                self.pos += 1;
                Node::Class {
                    negated: false,
                    items: vec![ClassItem::Space],
                }
            }
            'S' => {
                self.pos += 1;
                Node::Class {
                    negated: false,
                    items: vec![ClassItem::NotSpace],
                }
            }
            'b' => {
                self.pos += 1;
                Node::WordBoundary(true)
            }
            'B' => {
                self.pos += 1;
                Node::WordBoundary(false)
            }
            'n' => {
                self.pos += 1;
                Node::Char('\n')
            }
            't' => {
                self.pos += 1;
                Node::Char('\t')
            }
            'r' => {
                self.pos += 1;
                Node::Char('\r')
            }
            'f' => {
                self.pos += 1;
                Node::Char('\u{000C}')
            }
            'v' => {
                self.pos += 1;
                Node::Char('\u{000B}')
            }
            'c' => {
                // \cX control escape; if not a valid control letter, treat the
                // backslash literally (Annex B leniency: `\c` -> '\'). In unicode
                // mode an incomplete `\c` is a SyntaxError.
                self.pos += 1;
                match self.peek() {
                    Some(ch) if ch.is_ascii_alphabetic() => {
                        self.pos += 1;
                        Node::Char(((ch as u8) & 0x1f) as char)
                    }
                    _ if self.unicode => return Err("Invalid \\c escape".to_string()),
                    _ => Node::Char('\\'),
                }
            }
            'x' => {
                self.pos += 1;
                match self.parse_hex(2) {
                    Some(ch) => Node::Char(ch),
                    None if self.unicode => return Err("Invalid \\x escape".to_string()),
                    None => Node::Char('x'),
                }
            }
            'u' => {
                self.pos += 1;
                match self.parse_unicode_escape() {
                    Some(ch) => Node::Char(ch),
                    None if self.unicode => return Err("Invalid \\u escape".to_string()),
                    None => Node::Char('u'),
                }
            }
            'k' => {
                // Named backreference `\k<name>`.
                self.pos += 1;
                if self.peek() != Some('<') {
                    if self.unicode {
                        return Err("Invalid \\k escape".to_string());
                    }
                    // Annex B: `\k` not followed by `<` is an identity escape.
                    return Ok(Node::Char('k'));
                }
                self.pos += 1; // consume '<'
                let name = self.parse_group_name()?;
                // Record a placeholder; resolve after the whole pattern parses.
                let slot = self.named_backref_targets.len();
                self.named_backref_targets.push(0);
                self.pending_named_backrefs.push((name, slot));
                Node::Backref(named_backref_placeholder(slot))
            }
            'p' | 'P' => {
                // In Unicode mode `\p{...}`/`\P{...}` are property escapes resolved
                // against the Unicode tables. Without the `u`/`v` flag, `\p` is an
                // identity escape for the letter itself (Annex B).
                self.pos += 1; // consume 'p'/'P'
                if self.unicode {
                    let negated = c == 'P';
                    let ranges = self.parse_unicode_property()?;
                    Node::Class {
                        negated: false,
                        items: vec![ClassItem::Unicode { ranges, negated }],
                    }
                } else {
                    Node::Char(c)
                }
            }
            '0' => {
                // `\0` is NUL unless followed by a digit, in which case it is an
                // octal escape (Annex B). In unicode mode octal escapes are
                // forbidden: `\0` followed by a digit is a SyntaxError.
                if self.unicode && matches!(self.src.get(self.pos + 1), Some(d) if d.is_ascii_digit())
                {
                    return Err("Invalid octal escape in unicode mode".to_string());
                }
                Node::Char(self.parse_octal_escape())
            }
            '1'..='9' => {
                // Backreference. Consume the full decimal run. Clamp to a safe
                // ceiling well below the placeholder space used by named
                // backrefs (a backref that exceeds the group count never
                // participates anyway, so the exact value past the cap is moot).
                self.pos += 1;
                let mut n = (c as u8 - b'0') as usize;
                while let Some(c2) = self.peek() {
                    if c2.is_ascii_digit() {
                        n = n
                            .saturating_mul(10)
                            .saturating_add((c2 as u8 - b'0') as usize)
                            .min(BACKREF_CAP);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Node::Backref(n)
            }
            other => {
                // Identity escape: `\.` -> '.', `\/` -> '/', etc. In unicode mode
                // only SyntaxCharacters (and `/`) may be escaped this way; any
                // other identity escape is a SyntaxError.
                if self.unicode && !is_syntax_char(other) && other != '/' {
                    return Err(format!("Invalid identity escape '\\{other}' in unicode mode"));
                }
                self.pos += 1;
                Node::Char(other)
            }
        })
    }

    /// Parse a (possibly multi-digit) octal escape. The caller has positioned us
    /// at the first octal digit. Consumes 1..=3 octal digits with value <= 0o377.
    fn parse_octal_escape(&mut self) -> char {
        let mut v: u32 = 0;
        let mut count = 0;
        while count < 3 {
            match self.peek() {
                Some(c) if ('0'..='7').contains(&c) => {
                    let d = (c as u8 - b'0') as u32;
                    let next = v * 8 + d;
                    if next > 0o377 {
                        break;
                    }
                    v = next;
                    self.pos += 1;
                    count += 1;
                }
                _ => break,
            }
        }
        char::from_u32(v).unwrap_or('\0')
    }

    fn parse_hex(&mut self, count: usize) -> Option<char> {
        let save = self.pos;
        let mut v: u32 = 0;
        for _ in 0..count {
            let c = match self.peek() {
                Some(c) => c,
                None => {
                    self.pos = save;
                    return None;
                }
            };
            let d = match c.to_digit(16) {
                Some(d) => d,
                None => {
                    self.pos = save;
                    return None;
                }
            };
            v = v * 16 + d;
            self.pos += 1;
        }
        match char::from_u32(v) {
            Some(c) => Some(c),
            None => {
                self.pos = save;
                None
            }
        }
    }

    fn parse_unicode_escape(&mut self) -> Option<char> {
        if self.peek() == Some('{') {
            let save = self.pos;
            self.pos += 1;
            let mut v: u32 = 0;
            let mut any = false;
            while let Some(c) = self.peek() {
                if let Some(d) = c.to_digit(16) {
                    v = v.saturating_mul(16).saturating_add(d);
                    self.pos += 1;
                    any = true;
                } else {
                    break;
                }
            }
            if !any || !self.eat('}') {
                self.pos = save;
                return None;
            }
            match char::from_u32(v) {
                Some(c) => Some(c),
                None => {
                    self.pos = save;
                    None
                }
            }
        } else {
            self.parse_hex(4)
        }
    }
}

/// Decimal backreferences are clamped to this ceiling so they can never enter
/// the high half of the index space reserved for named-backref placeholders.
const BACKREF_CAP: usize = 1_000_000_000;

/// Named backreferences are stored as `Backref` nodes carrying a sentinel index
/// that encodes a placeholder slot. We pick a high value unlikely to collide
/// with a real group index; resolution rewrites them in place.
fn named_backref_placeholder(slot: usize) -> usize {
    usize::MAX - slot
}

fn is_named_backref_placeholder(idx: usize) -> Option<usize> {
    // Reserve the top portion of the index space for placeholders. Real group
    // counts are bounded by pattern length and never approach usize::MAX/2.
    if idx > usize::MAX / 2 {
        Some(usize::MAX - idx)
    } else {
        None
    }
}

/// Rewrite placeholder named-backref nodes with their resolved group indices.
fn resolve_named_backrefs(node: &mut Node, targets: &[usize]) {
    match node {
        Node::Backref(i) => {
            if let Some(slot) = is_named_backref_placeholder(*i) {
                *i = targets.get(slot).copied().unwrap_or(0);
            }
        }
        Node::Group { node, .. } | Node::Look { node, .. } => {
            resolve_named_backrefs(node, targets)
        }
        Node::Repeat { node, .. } => resolve_named_backrefs(node, targets),
        Node::Alt(branches) | Node::Concat(branches) => {
            for b in branches {
                resolve_named_backrefs(b, targets);
            }
        }
        _ => {}
    }
}

enum ClassAtom {
    Char(char),
    Class(ClassItem),
}

fn is_id_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

fn is_id_continue(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphanumeric() || c == '\u{200C}' || c == '\u{200D}'
}

/// Walk the AST checking for a numeric backreference whose target group index
/// exceeds the total group count (a SyntaxError in unicode mode).
fn has_out_of_range_backref(node: &Node, group_count: usize) -> bool {
    match node {
        Node::Backref(n) => *n > group_count,
        Node::Group { node, .. } | Node::Look { node, .. } | Node::Repeat { node, .. } => {
            has_out_of_range_backref(node, group_count)
        }
        Node::Alt(items) | Node::Concat(items) => {
            items.iter().any(|n| has_out_of_range_backref(n, group_count))
        }
        _ => false,
    }
}

/// True if `node` is an assertion (anchor, word boundary, or lookaround), which
/// cannot be quantified in unicode mode.
fn is_assertion(node: &Node) -> bool {
    matches!(
        node,
        Node::Start | Node::End | Node::WordBoundary(_) | Node::Look { .. }
    )
}

/// The RegExp `SyntaxCharacter` set: characters that may follow `\` as an
/// identity escape in unicode mode.
fn is_syntax_char(c: char) -> bool {
    matches!(
        c,
        '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
    )
}

// =========================================================================
// Compiled regex + matcher
// =========================================================================

struct Regex {
    root: Node,
    flags: Flags,
    group_count: usize,
    /// `(name, group index)` for each named capture group, declaration order.
    group_names: Vec<(String, usize)>,
}

/// Mutable capture state during a match attempt. `slots[i]` holds the span for
/// group `i` (group 0 reserved for the whole match, filled by the caller).
type Caps = Vec<Option<(usize, usize)>>;

impl Regex {
    fn compile(pattern: &str, flags: &str) -> Result<Regex, String> {
        let flags = Flags::parse(flags)?;
        let chars: Vec<char> = pattern.chars().collect();
        let mut parser = Parser::new(&chars, flags.unicode);
        let root = parser.parse()?;
        Ok(Regex {
            root,
            flags,
            group_count: parser.group_count,
            group_names: parser.group_names,
        })
    }

    fn exec(&self, input: &[char], start: usize) -> Option<ReMatch> {
        let mut at = start.min(input.len() + 1);
        // One shared step budget across the whole search bounds catastrophic
        // backtracking (e.g. /(a*)*b/ on a long non-matching input).
        let ctx = MatchCtx {
            input,
            flags: self.flags,
            steps: std::cell::Cell::new(0),
        };
        loop {
            if at > input.len() || ctx.steps.get() > REGEX_STEP_LIMIT {
                return None;
            }
            let mut caps: Caps = vec![None; self.group_count + 1];
            if let Some(end) = ctx.match_node(&self.root, at, &mut caps, &|pos, _caps| Some(pos)) {
                caps[0] = Some((at, end));
                return Some(ReMatch {
                    start: at,
                    end,
                    groups: caps,
                });
            }
            if self.flags.sticky {
                return None;
            }
            at += 1;
        }
    }
}

/// Backtracking-step ceiling per `exec` call. Bounds pathological patterns.
const REGEX_STEP_LIMIT: u64 = 100_000;

struct MatchCtx<'a> {
    input: &'a [char],
    flags: Flags,
    steps: std::cell::Cell<u64>,
}

/// A continuation: given the position reached after this node and the current
/// capture state, attempt to match the remainder and return the final end
/// position on success.
type Cont<'k> = dyn Fn(usize, &mut Caps) -> Option<usize> + 'k;

impl<'a> MatchCtx<'a> {
    fn char_eq(&self, a: char, b: char) -> bool {
        if a == b {
            return true;
        }
        if self.flags.ignore_case {
            // Canonicalize via simple case folding (lower/upper). Sufficient for
            // the common ASCII + Latin cases; full Unicode case folding is a
            // long-tail item.
            return fold(a) == fold(b);
        }
        false
    }

    fn match_node(
        &self,
        node: &Node,
        pos: usize,
        caps: &mut Caps,
        k: &Cont<'_>,
    ) -> Option<usize> {
        let s = self.steps.get();
        if s > REGEX_STEP_LIMIT {
            return None;
        }
        self.steps.set(s + 1);
        match node {
            Node::Empty => k(pos, caps),
            Node::Char(c) => {
                if pos < self.input.len() && self.char_eq(self.input[pos], *c) {
                    k(pos + 1, caps)
                } else {
                    None
                }
            }
            Node::AnyChar => {
                if pos < self.input.len() {
                    let ch = self.input[pos];
                    if self.flags.dot_all || !is_line_terminator(ch) {
                        return k(pos + 1, caps);
                    }
                }
                None
            }
            Node::Class { negated, items } => {
                if pos < self.input.len() {
                    let ch = self.input[pos];
                    let matched = class_matches(items, ch, self.flags.ignore_case);
                    if matched != *negated {
                        return k(pos + 1, caps);
                    }
                }
                None
            }
            Node::Start => {
                if pos == 0 || (self.flags.multiline && is_line_terminator(self.input[pos - 1])) {
                    k(pos, caps)
                } else {
                    None
                }
            }
            Node::End => {
                if pos == self.input.len()
                    || (self.flags.multiline && is_line_terminator(self.input[pos]))
                {
                    k(pos, caps)
                } else {
                    None
                }
            }
            Node::WordBoundary(want) => {
                let before = pos > 0 && is_word_char(self.input[pos - 1]);
                let after = pos < self.input.len() && is_word_char(self.input[pos]);
                let boundary = before != after;
                if boundary == *want {
                    k(pos, caps)
                } else {
                    None
                }
            }
            Node::Group { idx, node } => {
                match idx {
                    None => self.match_node(node, pos, caps, k),
                    Some(i) => {
                        let i = *i;
                        let saved = caps[i];
                        let start = pos;
                        // Continuation records the capture span before invoking
                        // the outer continuation, restoring on failure.
                        let inner_k = move |end: usize, caps: &mut Caps| {
                            let prev = caps[i];
                            caps[i] = Some((start, end));
                            match k(end, caps) {
                                Some(r) => Some(r),
                                None => {
                                    caps[i] = prev;
                                    None
                                }
                            }
                        };
                        match self.match_node(node, pos, caps, &inner_k) {
                            Some(r) => Some(r),
                            None => {
                                caps[i] = saved;
                                None
                            }
                        }
                    }
                }
            }
            Node::Look {
                behind,
                negative,
                node,
            } => self.match_look(*behind, *negative, node, pos, caps, k),
            Node::Alt(branches) => {
                for b in branches {
                    if let Some(r) = self.match_node(b, pos, caps, k) {
                        return Some(r);
                    }
                }
                None
            }
            Node::Concat(items) => self.match_seq(items, 0, pos, caps, k),
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => self.match_repeat(node, *min, *max, *greedy, 0, pos, caps, k),
            Node::Backref(i) => {
                let span = if *i < caps.len() { caps[*i] } else { None };
                match span {
                    // An unparticipating group matches the empty string.
                    None => k(pos, caps),
                    Some((s, e)) => {
                        let len = e - s;
                        if pos + len > self.input.len() {
                            return None;
                        }
                        for j in 0..len {
                            if !self.char_eq(self.input[pos + j], self.input[s + j]) {
                                return None;
                            }
                        }
                        k(pos + len, caps)
                    }
                }
            }
        }
    }

    /// Match a lookaround assertion. Lookahead tries to match `node` starting at
    /// `pos`; lookbehind tries to match `node` over some span ending exactly at
    /// `pos` (scanning candidate start positions). For positive assertions any
    /// captures set inside survive into the continuation; for negative
    /// assertions no captures are kept and success means *no* match was found.
    fn match_look(
        &self,
        behind: bool,
        negative: bool,
        node: &Node,
        pos: usize,
        caps: &mut Caps,
        k: &Cont<'_>,
    ) -> Option<usize> {
        // Snapshot so we can roll captures back. Negative assertions must never
        // expose captures; positive assertions must roll back if the outer
        // continuation backtracks through us.
        let snapshot = caps.clone();
        let matched = if !behind {
            // Lookahead: match the sub-pattern at `pos`, ignoring where it ends.
            let stop = |p: usize, _c: &mut Caps| Some(p);
            self.match_node(node, pos, caps, &stop).is_some()
        } else {
            // Lookbehind: succeed iff the sub-pattern matches some span that ends
            // exactly at `pos`. Try every candidate start `j` in `0..=pos`,
            // longest span first for deterministic capture behaviour.
            let mut hit = false;
            let mut j = pos + 1;
            while j > 0 {
                j -= 1;
                if self.steps.get() > REGEX_STEP_LIMIT {
                    break;
                }
                let stop = move |p: usize, _c: &mut Caps| {
                    if p == pos {
                        Some(p)
                    } else {
                        None
                    }
                };
                if self.match_node(node, j, caps, &stop).is_some() {
                    hit = true;
                    break;
                }
            }
            hit
        };

        if negative {
            // Negative assertions discard any captures the sub-pattern touched.
            *caps = snapshot;
            if matched {
                None
            } else {
                k(pos, caps)
            }
        } else if matched {
            match k(pos, caps) {
                Some(r) => Some(r),
                None => {
                    // Backtracked past us: undo the assertion's captures.
                    *caps = snapshot;
                    None
                }
            }
        } else {
            *caps = snapshot;
            None
        }
    }

    fn match_seq(
        &self,
        items: &[Node],
        i: usize,
        pos: usize,
        caps: &mut Caps,
        k: &Cont<'_>,
    ) -> Option<usize> {
        if i >= items.len() {
            return k(pos, caps);
        }
        let rest_k = move |p: usize, caps: &mut Caps| self.match_seq(items, i + 1, p, caps, k);
        self.match_node(&items[i], pos, caps, &rest_k)
    }

    #[allow(clippy::too_many_arguments)]
    /// Whether `node` matches exactly one input char and never backtracks
    /// internally — eligible for the iterative quantifier fast path.
    fn node_matches_one(&self, node: &Node, pos: usize) -> bool {
        if pos >= self.input.len() {
            return false;
        }
        let ch = self.input[pos];
        match node {
            Node::Char(c) => self.char_eq(ch, *c),
            Node::AnyChar => self.flags.dot_all || !is_line_terminator(ch),
            Node::Class { negated, items } => {
                class_matches(items, ch, self.flags.ignore_case) != *negated
            }
            _ => false,
        }
    }

    fn match_repeat(
        &self,
        node: &Node,
        min: usize,
        max: Option<usize>,
        greedy: bool,
        done: usize,
        pos: usize,
        caps: &mut Caps,
        k: &Cont<'_>,
    ) -> Option<usize> {
        // Fast path: a quantifier over a single-char-consuming atom (a literal,
        // `.`, or a character class — none of which capture or backtrack
        // internally) is matched in a tight iterative loop instead of the
        // per-repetition CPS recursion below. The recursion both overflows the
        // native stack and burns the step budget on large inputs (the
        // property-escape sweeps run `/^\p{...}+$/u` over ~1M chars), so this is
        // what makes those matches feasible at all.
        if is_simple_one_char(node) {
            let cap = max.unwrap_or(usize::MAX);
            let mut p = pos;
            let mut total = done;
            while total < cap && self.node_matches_one(node, p) {
                p += 1;
                total += 1;
            }
            let floor = min.max(done);
            if total < floor {
                return None;
            }
            if greedy {
                let mut tn = total;
                let mut tp = p;
                loop {
                    if let Some(r) = k(tp, caps) {
                        return Some(r);
                    }
                    if tn == floor {
                        return None;
                    }
                    tn -= 1;
                    tp -= 1;
                }
            } else {
                let mut tn = floor;
                let mut tp = pos + (floor - done);
                loop {
                    if let Some(r) = k(tp, caps) {
                        return Some(r);
                    }
                    if tn == total {
                        return None;
                    }
                    tn += 1;
                    tp += 1;
                }
            }
        }
        // Still obligated to match more.
        if done < min {
            let more = move |p: usize, caps: &mut Caps| {
                // Guard against zero-width infinite loops.
                if p == pos && done >= 1 {
                    return None;
                }
                self.match_repeat(node, min, max, greedy, done + 1, p, caps, k)
            };
            return self.match_node(node, pos, caps, &more);
        }
        let at_max = matches!(max, Some(m) if done >= m);

        if greedy {
            if !at_max {
                let more = move |p: usize, caps: &mut Caps| {
                    if p == pos {
                        // zero-width iteration: stop expanding to avoid looping.
                        return None;
                    }
                    self.match_repeat(node, min, max, greedy, done + 1, p, caps, k)
                };
                if let Some(r) = self.match_node(node, pos, caps, &more) {
                    return Some(r);
                }
            }
            k(pos, caps)
        } else {
            // Lazy: try to stop first.
            if let Some(r) = k(pos, caps) {
                return Some(r);
            }
            if !at_max {
                let more = move |p: usize, caps: &mut Caps| {
                    if p == pos {
                        return None;
                    }
                    self.match_repeat(node, min, max, greedy, done + 1, p, caps, k)
                };
                return self.match_node(node, pos, caps, &more);
            }
            None
        }
    }
}

/// A node that consumes exactly one input char with no internal backtracking,
/// so a `*`/`+`/`{n,m}` over it can be matched iteratively (see `match_repeat`).
fn is_simple_one_char(node: &Node) -> bool {
    matches!(node, Node::Char(_) | Node::AnyChar | Node::Class { .. })
}

fn class_matches(items: &[ClassItem], ch: char, ignore_case: bool) -> bool {
    for item in items {
        let hit = match item {
            ClassItem::Char(c) => ch == *c || (ignore_case && fold(ch) == fold(*c)),
            ClassItem::Range(lo, hi) => {
                let in_range = (*lo as u32) <= (ch as u32) && (ch as u32) <= (*hi as u32);
                if in_range {
                    true
                } else if ignore_case {
                    let f = fold(ch);
                    ((*lo as u32) <= (f as u32) && (f as u32) <= (*hi as u32)) || {
                        // Also try folding the bounds for ASCII letter ranges.
                        let u = ch.to_ascii_uppercase();
                        let l = ch.to_ascii_lowercase();
                        ((*lo as u32) <= (u as u32) && (u as u32) <= (*hi as u32))
                            || ((*lo as u32) <= (l as u32) && (l as u32) <= (*hi as u32))
                    }
                } else {
                    false
                }
            }
            ClassItem::Digit => ch.is_ascii_digit(),
            ClassItem::NotDigit => !ch.is_ascii_digit(),
            ClassItem::Word => is_word_char(ch),
            ClassItem::NotWord => !is_word_char(ch),
            ClassItem::Space => is_regex_space(ch),
            ClassItem::NotSpace => !is_regex_space(ch),
            ClassItem::Unicode { ranges, negated } => {
                // `ranges` is sorted and non-overlapping (from the generated
                // Unicode 17.0 tables), so a
                // binary search keeps membership O(log n) — important for the
                // `/^\p{...}+$/u`-over-a-huge-string property-escapes tests.
                let inside = ranges
                    .binary_search_by(|(lo, hi)| {
                        if ch < *lo {
                            std::cmp::Ordering::Greater
                        } else if ch > *hi {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Equal
                        }
                    })
                    .is_ok();
                inside != *negated
            }
        };
        if hit {
            return true;
        }
    }
    false
}

fn fold(c: char) -> char {
    // Simple case fold: map to lowercase. Adequate for ASCII / common Latin.
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Resolve a `\p{...}` property name to code-point ranges via the generated
/// Unicode 17.0 tables. `name` is the brace content: a bare name (a
/// General_Category, Script, or binary property — UTS#18 §1.2: `\p{Letter}`,
/// `\p{L}`, `\p{Greek}`, `\p{White_Space}`) or `Property=Value`
/// (`\p{Script=Latin}`, `\p{gc=Nd}`). Returns an error (→ SyntaxError) for an
/// unknown property, per spec.
fn resolve_unicode_property(name: &str) -> Result<std::rc::Rc<Vec<(char, char)>>, String> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    // Memoize: each property's range set (a `Vec<(char, char)>` built from the
    // static `&[(u32, u32)]` slice) is built once and shared as an `Rc`. The
    // engine re-parses a regex pattern on every match call, so without this
    // cache a `\p{...}` regex looped over many inputs would rebuild it each time.
    thread_local! {
        static CACHE: RefCell<HashMap<String, Option<Rc<Vec<(char, char)>>>>> =
            RefCell::new(HashMap::new());
    }
    let err = || format!("Invalid property name in regular expression: \\p{{{name}}}");
    if let Some(hit) = CACHE.with(|c| c.borrow().get(name).cloned()) {
        return hit.ok_or_else(err);
    }
    let resolved = crate::unicode_tables::lookup(name).map(|ranges| {
        Rc::new(
            ranges
                .iter()
                // The generated tables only contain valid scalar/surrogate code
                // points; `from_u32` can only fail for surrogates, which the
                // gc=Cs / Any tables never reach as match targets (JS strings
                // are UTF-16, but the matcher iterates scalar values). Clamp
                // defensively by skipping any unrepresentable endpoint pair.
                .filter_map(|&(lo, hi)| {
                    Some((char::from_u32(lo)?, char::from_u32(hi)?))
                })
                .collect(),
        )
    });
    CACHE.with(|c| c.borrow_mut().insert(name.to_string(), resolved.clone()));
    resolved.ok_or_else(err)
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_regex_space(c: char) -> bool {
    // \s per spec: WhiteSpace + LineTerminator.
    matches!(
        c,
        ' ' | '\t'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{00A0}'
            | '\u{FEFF}'
            | '\n'
            | '\r'
            | '\u{2028}'
            | '\u{2029}'
    ) || c.is_whitespace()
}

// =========================================================================
// make_regexp — build the RegExp object the compiler/runtime use
// =========================================================================

/// Hidden, non-enumerable marker key used to brand-check RegExp objects without
/// a dedicated `Internal` slot.
pub const REGEXP_MARK: &str = "[[IsRegExp]]";

impl Vm {
    pub fn make_regexp(&mut self, pattern: &str, flags: &str) -> Result<Value, Value> {
        // Validate flags and pattern; report unsupported constructs as SyntaxError.
        if let Err(msg) = regex_is_valid(pattern, flags) {
            return Err(self.throw_syntax(&msg));
        }
        let source = if pattern.is_empty() { "(?:)" } else { pattern };
        let o = JsObject::new(ObjectData::new(
            Some(self.realm.regexp_proto.clone()),
            Internal::Ordinary,
        ));
        {
            let mut b = o.borrow_mut();
            // Hidden brand marker.
            b.props.insert(
                PropertyKey::str(REGEXP_MARK),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Bool(true),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
            // Internal source/flags strings used by exec to re-parse per call.
            b.props.insert(
                PropertyKey::str("[[Source]]"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::str(source),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
            b.props.insert(
                PropertyKey::str("[[Flags]]"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::str(flags),
                        writable: false,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
            // The writable lastIndex data property.
            b.props.insert(
                PropertyKey::str("lastIndex"),
                Property {
                    kind: PropertyKind::Data {
                        value: Value::Number(0.0),
                        writable: true,
                    },
                    enumerable: false,
                    configurable: false,
                },
            );
        }
        Ok(Value::Object(o))
    }
}

/// True if `v` is a RegExp object built by `make_regexp`.
pub fn is_regexp(v: &Value) -> bool {
    if let Value::Object(o) = v {
        return o
            .borrow()
            .props
            .contains_key(&PropertyKey::str(REGEXP_MARK));
    }
    false
}

/// Read the stored source/flags strings off a RegExp object.
pub fn regexp_source_flags(o: &JsObject) -> (String, String) {
    let b = o.borrow();
    let source = b
        .props
        .get(&PropertyKey::str("[[Source]]"))
        .and_then(|p| p.value())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let flags = b
        .props
        .get(&PropertyKey::str("[[Flags]]"))
        .and_then(|p| p.value())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (source, flags)
}
