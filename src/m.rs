//! Power Query M parser — token based.
//!
//! The previous line scanner only recognised `let` / `in` on lines of their
//! own, one `name = …` per line and a single output line, so inline queries
//! (`let x = 1 in x`), continuation lines and multi-line outputs silently lost
//! their semantic payload (#11).  This module tokenises the whole document
//! (strings, quoted identifiers, comments, brackets) and splits `let`
//! bindings on depth-0 commas and the matching `in`, so every expression byte
//! that is not layout or a comment reaches a label.
//!
//! Semantic nodes:
//!   m_file            — root
//!   query             — `let … in …` (label = query_N)
//!   step              — binding inside a let (label = binding name)
//!   step_expression   — right-hand side of a binding
//!   step_fragment     — binding text that is not `name = expression`
//!   query_output      — expression after `in`
//!   expression        — top-level expression that is not a let
//!   section           — `section Name;` document (label = section name)
//!   section_member    — `[shared] Name = expression;` (label = name)
//!   modifier          — `shared` on a section member
//!   member_expression — non-let section member body
//!   section_fragment  — unrecognised section member text
//!
//! Labels are rebuilt from token text joined by a single space wherever the
//! source had whitespace or a comment between two tokens, so re-indentation,
//! line breaks and comments do not alter hashes while every literal, operator
//! and identifier does.  Positions are 0-based rows and UTF-8 byte columns of
//! the first and last token of each node.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Ident,
    QuotedIdent,
    Str,
    Num,
    Punct,
    Op,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    kind: Kind,
    start: usize,
    end: usize,
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic() || (!c.is_ascii() && !c.is_whitespace())
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c == '.' || c.is_alphanumeric() || (!c.is_ascii() && !c.is_whitespace())
}

/// Index just past a `"…"` literal whose opening quote is at `open`.
/// `""` is an escaped quote; an unterminated literal runs to end of input.
fn scan_quoted(b: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if b[i] == b'"' {
            if i + 1 < b.len() && b[i + 1] == b'"' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn char_at(src: &str, i: usize) -> char {
    src[i..].chars().next().unwrap_or('\0')
}

fn tokenize(src: &str) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = char_at(src, i);
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i < b.len() && !(b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        let start = i;
        let kind;
        if b[i] == b'"' {
            i = scan_quoted(b, i);
            kind = Kind::Str;
        } else if b[i] == b'#' && i + 1 < b.len() && b[i + 1] == b'"' {
            i = scan_quoted(b, i + 1);
            kind = Kind::QuotedIdent;
        } else if b[i] == b'#' && i + 1 < b.len() && is_ident_start(char_at(src, i + 1)) {
            i += 1;
            while i < b.len() && is_ident_continue(char_at(src, i)) {
                i += char_at(src, i).len_utf8();
            }
            kind = Kind::Ident;
        } else if is_ident_start(c) {
            while i < b.len() && is_ident_continue(char_at(src, i)) {
                i += char_at(src, i).len_utf8();
            }
            // `x..y` is a range: the trailing dots belong to the operator.
            while i > start + 1 && b[i - 1] == b'.' {
                i -= 1;
            }
            kind = Kind::Ident;
        } else if b[i].is_ascii_digit()
            || (b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit())
        {
            if b[i] == b'0' && i + 1 < b.len() && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    let mut j = i + 1;
                    if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                        j += 1;
                    }
                    if j < b.len() && b[j].is_ascii_digit() {
                        i = j;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
            }
            kind = Kind::Num;
        } else if matches!(b[i], b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b';') {
            i += 1;
            kind = Kind::Punct;
        } else {
            let rest = &src[i..];
            let len = ["...", "..", "=>", "<=", ">=", "<>", "??"]
                .iter()
                .find(|op| rest.starts_with(**op))
                .map(|op| op.len())
                .unwrap_or(c.len_utf8());
            i += len;
            kind = Kind::Op;
        }
        toks.push(Tok {
            kind,
            start,
            end: i,
        });
    }
    toks
}

struct MParser<'a> {
    src: &'a str,
    toks: Vec<Tok>,
    line_starts: Vec<usize>,
    query_counter: usize,
}

impl<'a> MParser<'a> {
    fn new(src: &'a str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        MParser {
            src,
            toks: tokenize(src),
            line_starts,
            query_counter: 0,
        }
    }

    fn pos(&self, offset: usize) -> (u32, u32) {
        let line = self
            .line_starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1);
        (line as u32, (offset - self.line_starts[line]) as u32)
    }

    fn text(&self, i: usize) -> &'a str {
        &self.src[self.toks[i].start..self.toks[i].end]
    }

    fn is_word(&self, i: usize, word: &str) -> bool {
        i < self.toks.len() && self.toks[i].kind == Kind::Ident && self.text(i) == word
    }

    fn is_punct(&self, i: usize, p: &str) -> bool {
        i < self.toks.len() && self.toks[i].kind == Kind::Punct && self.text(i) == p
    }

    fn first_on_line(&self, i: usize) -> bool {
        i == 0 || self.pos(self.toks[i - 1].start).0 != self.pos(self.toks[i].start).0
    }

    /// Normalised text of tokens `a..b`: token bytes, one space per gap.
    fn label(&self, a: usize, b: usize) -> String {
        let mut out = String::new();
        for k in a..b {
            if k > a && self.toks[k].start > self.toks[k - 1].end {
                out.push(' ');
            }
            out.push_str(self.text(k));
        }
        out
    }

    fn place(&self, mut node: SemanticNode, a: usize, b: usize) -> SemanticNode {
        if a < b {
            let (sl, sc) = self.pos(self.toks[a].start);
            let (el, ec) = self.pos(self.toks[b - 1].end);
            node.position.start_line = sl;
            node.position.start_col = sc;
            node.position.end_line = el;
            node.position.end_col = ec;
        }
        node
    }

    fn leaf(&self, id: String, node_type: &str, a: usize, b: usize) -> SemanticNode {
        let label = self.label(a, b);
        let node =
            SemanticNodeBuilder::new(id, node_type, label, 0, 0, 0, 0, String::new()).build();
        self.place(node, a, b)
    }

    fn block(
        &self,
        id: String,
        node_type: &str,
        label: String,
        a: usize,
        b: usize,
        children: Vec<SemanticNode>,
    ) -> SemanticNode {
        let node = SemanticNodeBuilder::new(id, node_type, label, 0, 0, 0, 0, String::new())
            .children(children)
            .build();
        self.place(node, a, b)
    }

    fn bracket_delta(&self, i: usize) -> i32 {
        if self.toks[i].kind != Kind::Punct {
            return 0;
        }
        match self.text(i) {
            "(" | "[" | "{" => 1,
            ")" | "]" | "}" => -1,
            _ => 0,
        }
    }

    /// End of the binding starting at `a`: the first depth-0 `,` or the `in`
    /// that closes the enclosing let (nested `let … in` pairs are skipped).
    fn binding_end(&self, a: usize, end: usize) -> usize {
        let mut depth = 0i32;
        let mut lets = 0usize;
        let mut k = a;
        while k < end {
            if depth == 0 {
                if self.is_punct(k, ",") && lets == 0 {
                    return k;
                }
                if self.is_word(k, "let") {
                    lets += 1;
                } else if self.is_word(k, "in") {
                    if lets == 0 {
                        return k;
                    }
                    lets -= 1;
                }
            }
            depth = (depth + self.bracket_delta(k)).max(0);
            k += 1;
        }
        end
    }

    /// End of a top-level expression starting at `a`: the next depth-0 `let`
    /// that begins a line (a following query), or `end`.
    fn top_expression_end(&self, a: usize, end: usize) -> usize {
        let mut depth = 0i32;
        let mut k = a;
        while k < end {
            if k > a && depth == 0 && self.is_word(k, "let") && self.first_on_line(k) {
                return k;
            }
            depth = (depth + self.bracket_delta(k)).max(0);
            k += 1;
        }
        end
    }

    fn quoted_name(&self, i: usize) -> String {
        let t = self.text(i);
        if self.toks[i].kind == Kind::QuotedIdent {
            let inner = t.strip_prefix("#\"").unwrap_or(t);
            let inner = inner.strip_suffix('"').unwrap_or(inner);
            inner.replace("\"\"", "\"")
        } else {
            t.to_string()
        }
    }

    fn is_name(&self, i: usize) -> bool {
        i < self.toks.len() && matches!(self.toks[i].kind, Kind::Ident | Kind::QuotedIdent)
    }

    fn is_equals(&self, i: usize) -> bool {
        i < self.toks.len() && self.toks[i].kind == Kind::Op && self.text(i) == "="
    }

    fn step(&self, id: String, a: usize, b: usize) -> SemanticNode {
        if b - a >= 2 && self.is_name(a) && self.is_equals(a + 1) {
            let mut children = Vec::new();
            if a + 2 < b {
                children.push(self.leaf(format!("{}.0", id), "step_expression", a + 2, b));
            }
            self.block(id, "step", self.quoted_name(a), a, b, children)
        } else {
            self.leaf(id, "step_fragment", a, b)
        }
    }

    /// Parse `let … in …` beginning at token `a` (the `let`).  Returns the
    /// query node and the index after the last consumed token.
    fn query(
        &mut self,
        id: String,
        a: usize,
        end: usize,
        top_level: bool,
    ) -> (SemanticNode, usize) {
        let label = format!("query_{}", self.query_counter);
        self.query_counter += 1;
        let mut children = Vec::new();
        let mut j = a + 1;
        let mut found_in = false;
        while j < end {
            if self.is_word(j, "in") {
                found_in = true;
                break;
            }
            let k = self.binding_end(j, end);
            if k > j {
                children.push(self.step(format!("{}.{}", id, children.len()), j, k));
            }
            if k < end && self.is_punct(k, ",") {
                j = k + 1;
            } else {
                j = k;
            }
        }
        let mut last = j;
        if found_in {
            let out_start = j + 1;
            let out_end = if top_level {
                self.top_expression_end(out_start, end)
            } else {
                end
            };
            if out_end > out_start {
                children.push(self.leaf(
                    format!("{}.{}", id, children.len()),
                    "query_output",
                    out_start,
                    out_end,
                ));
            }
            last = out_end.max(j + 1);
        }
        (self.block(id, "query", label, a, last, children), last)
    }

    /// Nodes for the expression tokens `a..end` (a let query or plain text).
    fn expressions(
        &mut self,
        prefix: &str,
        first_index: usize,
        a: usize,
        end: usize,
        plain_type: &str,
        top_level: bool,
    ) -> Vec<SemanticNode> {
        let mut nodes = Vec::new();
        let mut i = a;
        while i < end {
            let id = format!("{}.{}", prefix, first_index + nodes.len());
            if self.is_word(i, "let") {
                let (node, next) = self.query(id, i, end, top_level);
                nodes.push(node);
                i = next;
            } else {
                let next = if top_level {
                    self.top_expression_end(i, end)
                } else {
                    end
                };
                nodes.push(self.leaf(id, plain_type, i, next));
                i = next;
            }
        }
        nodes
    }

    fn section(&mut self) -> Vec<SemanticNode> {
        let end = self.toks.len();
        let mut header_end = 1;
        while header_end < end && !self.is_punct(header_end, ";") {
            header_end += 1;
        }
        let section_label = self.label(1, header_end);
        let mut members = Vec::new();
        let mut i = (header_end + 1).min(end);
        while i < end {
            let mut k = i;
            while k < end && !self.is_punct(k, ";") {
                k += 1;
            }
            if k > i {
                let id = format!("0.0.{}", members.len());
                members.push(self.member(id, i, k));
            }
            i = k + 1;
        }
        let close = (header_end + 1).min(end);
        let last = if end > close { end } else { close };
        vec![self.block(
            "0.0".to_string(),
            "section",
            section_label,
            0,
            last,
            members,
        )]
    }

    fn member(&mut self, id: String, a: usize, b: usize) -> SemanticNode {
        let shared = self.is_word(a, "shared");
        let n = if shared { a + 1 } else { a };
        if !(n + 1 < b && self.is_name(n) && self.is_equals(n + 1)) {
            return self.leaf(id, "section_fragment", a, b);
        }
        let mut children = Vec::new();
        if shared {
            children.push(self.leaf(format!("{}.0", id), "modifier", a, a + 1));
        }
        let first = children.len();
        let body = self.expressions(&id, first, n + 2, b, "member_expression", false);
        children.extend(body);
        self.block(id, "section_member", self.quoted_name(n), a, b, children)
    }

    fn parse(mut self) -> SemanticNode {
        let end = self.toks.len();
        let children = if end > 0 && self.is_word(0, "section") {
            self.section()
        } else {
            self.expressions("0", 0, 0, end, "expression", true)
        };
        let trimmed = self.src.trim_end().len();
        let (el, ec) = self.pos(trimmed);
        let mut root = SemanticNodeBuilder::new("0", "m_file", "m_file", 0, 0, 0, 0, String::new())
            .children(children)
            .build();
        root.position.end_line = el;
        root.position.end_col = ec;
        root
    }
}

pub(crate) fn parse_m(source: &str) -> String {
    let root = MParser::new(source).parse();
    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}
