//! Tree-sitter bash syntax highlighting and token cache.
//! Direct-mapped cache for zero-realloc AST tokenization.

use std::cell::RefCell;
use ratatui::style::{Color, Modifier, Style};
use tree_sitter::{Node, Parser};

use crate::arena::{fnv1a, Theme};

// ── Types ────────────────────────────────────────────────────────

/// A colored span within a command string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: u16,
    pub end:   u16,
    pub kind:  TokenKind,
}

/// Bash token categories for coloring.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TokenKind {
    Command,
    Flag,
    String,
    Variable,
    Operator,
    Comment,
    Plain,
}

/// Direct-mapped token cache (2048 slots).
pub struct TokenCache {
    slots: Box<[(u64, Vec<Span>); 2048]>,
}

/// Preview cache (128 slots) for syntax-highlighted multi-line blocks.
pub struct PreviewCache {
    slots: Box<[(u64, Vec<Span>); 128]>,
}

// ── Thread-local Parser ──────────────────────────────────────────

thread_local! {
    static BASH_PARSER: RefCell<Parser> = RefCell::new({
        let mut p = Parser::new();
        let lang = tree_sitter_bash::LANGUAGE;
        #[allow(clippy::expect_used)]
        p.set_language(&lang.into()).expect("valid bash grammar");
        p
    });
}

// ── TokenCache & PreviewCache ────────────────────────────────────

impl TokenCache {
    #[must_use]
    pub fn new() -> Self {
        let mut v: Vec<(u64, Vec<Span>)> = Vec::with_capacity(2048);
        for _ in 0..2048 {
            v.push((0, Vec::new()));
        }
        let Ok(slots) = v.into_boxed_slice().try_into() else {
            unreachable!();
        };
        Self { slots }
    }
}

impl Default for TokenCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PreviewCache {
    #[must_use]
    pub fn new() -> Self {
        let mut v: Vec<(u64, Vec<Span>)> = Vec::with_capacity(128);
        for _ in 0..128 {
            v.push((0, Vec::new()));
        }
        let Ok(slots) = v.into_boxed_slice().try_into() else {
            unreachable!();
        };
        Self { slots }
    }
}

impl Default for PreviewCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── AST Traversal & Mapping ──────────────────────────────────────

fn collect_spans(node: Node<'_>, cmd: &[u8], spans: &mut Vec<Span>) {
    let kind = node.kind();
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();

    if start_byte >= end_byte || end_byte > u16::MAX as usize {
        return;
    }

    #[allow(clippy::cast_possible_truncation)]
    let start = start_byte as u16;
    #[allow(clippy::cast_possible_truncation)]
    let end = end_byte as u16;

    // Check specific node types
    match kind {
        "command_name" => {
            spans.push(Span { start, end, kind: TokenKind::Command });
            return;
        }
        "comment" => {
            spans.push(Span { start, end, kind: TokenKind::Comment });
            return;
        }
        "string" | "raw_string" | "heredoc_body" | "heredoc_start" => {
            spans.push(Span { start, end, kind: TokenKind::String });
            return;
        }
        "variable_name" | "simple_expansion" | "expansion" => {
            spans.push(Span { start, end, kind: TokenKind::Variable });
            return;
        }
        "|" | "||" | "&&" | "&" | ";" | ";;" | ">" | ">>" | "<" | "<<" | "<<<" | ">&" | "<&" | "|&" | "$" => {
            spans.push(Span { start, end, kind: TokenKind::Operator });
            return;
        }
        "word" => {
            // Is it a flag? (Starts with '-')
            let slice = &cmd[start_byte..end_byte];
            if slice.starts_with(b"-") {
                spans.push(Span { start, end, kind: TokenKind::Flag });
            } else {
                spans.push(Span { start, end, kind: TokenKind::Plain });
            }
            return;
        }
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_spans(child, cmd, spans);
    }
}

fn parse_bash(cmd: &str, spans: &mut Vec<Span>) {
    BASH_PARSER.with(|parser_cell| {
        let mut parser = parser_cell.borrow_mut();
        if let Some(tree) = parser.parse(cmd, None) {
            collect_spans(tree.root_node(), cmd.as_bytes(), spans);
        }
    });
}

// ── Public API ───────────────────────────────────────────────────

/// Tokenize a command string into colored spans using tree-sitter.
/// Uses the direct-mapped cache to avoid re-parsing unchanged strings.
#[allow(clippy::cast_possible_truncation)]
pub fn tokenize<'a>(cache: &'a mut TokenCache, cmd: &str) -> &'a [Span] {
    if cmd.is_empty() {
        return &[];
    }
    let h = match fnv1a(cmd.as_bytes()) {
        0 => 1,
        other => other,
    };
    let slot = (h as usize) & (2048 - 1);
    if cache.slots[slot].0 == h {
        return &cache.slots[slot].1;
    }

    let mut spans = Vec::new();
    parse_bash(cmd, &mut spans);
    cache.slots[slot] = (h, spans);
    &cache.slots[slot].1
}

/// Produce full syntax-highlighted preview for a multi-line command.
#[allow(clippy::cast_possible_truncation)]
pub fn preview<'a>(cache: &'a mut PreviewCache, cmd: &str) -> &'a [Span] {
    if cmd.is_empty() {
        return &[];
    }
    let h = match fnv1a(cmd.as_bytes()) {
        0 => 1,
        other => other,
    };
    let slot = (h as usize) & (128 - 1);
    if cache.slots[slot].0 == h {
        return &cache.slots[slot].1;
    }

    let mut spans = Vec::new();
    parse_bash(cmd, &mut spans);
    cache.slots[slot] = (h, spans);
    &cache.slots[slot].1
}

/// Map `TokenKind` to ANSI color based on `Theme`.
#[must_use]
pub fn style_for(kind: TokenKind, theme: Theme) -> Style {
    match theme {
        Theme::Dark => match kind {
            TokenKind::Command => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            TokenKind::Flag => Style::default().fg(Color::Yellow),
            TokenKind::String => Style::default().fg(Color::Green),
            TokenKind::Variable => Style::default().fg(Color::Magenta),
            TokenKind::Operator => Style::default().fg(Color::LightRed),
            TokenKind::Comment => Style::default().fg(Color::DarkGray),
            TokenKind::Plain => Style::default().fg(Color::White),
        },
        Theme::Light => match kind {
            TokenKind::Command => Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            TokenKind::Flag => Style::default().fg(Color::Rgb(180, 100, 0)),
            TokenKind::String => Style::default().fg(Color::Green),
            TokenKind::Variable => Style::default().fg(Color::Magenta),
            TokenKind::Operator => Style::default().fg(Color::Red),
            TokenKind::Comment => Style::default().fg(Color::Gray),
            TokenKind::Plain => Style::default().fg(Color::Black),
        },
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let mut cache = TokenCache::new();
        let spans = tokenize(&mut cache, "ls foo");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].kind, TokenKind::Command);
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 2);
        assert_eq!(spans[1].kind, TokenKind::Plain);
        assert_eq!(spans[1].start, 3);
        assert_eq!(spans[1].end, 6);
    }

    #[test]
    fn pipeline_operator_spans() {
        let mut cache = TokenCache::new();
        let spans = tokenize(&mut cache, "ls | grep foo");
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].kind, TokenKind::Command);
        assert_eq!(spans[0].start, 0);
        assert_eq!(spans[0].end, 2);

        assert_eq!(spans[1].kind, TokenKind::Operator);
        assert_eq!(spans[1].start, 3);
        assert_eq!(spans[1].end, 4);

        assert_eq!(spans[2].kind, TokenKind::Command);
        assert_eq!(spans[2].start, 5);
        assert_eq!(spans[2].end, 9);

        assert_eq!(spans[3].kind, TokenKind::Plain);
        assert_eq!(spans[3].start, 10);
        assert_eq!(spans[3].end, 13);
    }

    #[test]
    fn flags_and_strings() {
        let mut cache = TokenCache::new();
        let spans = tokenize(&mut cache, "grep -rn \"pattern\" file");
        assert_eq!(spans[0].kind, TokenKind::Command);
        assert_eq!(spans[1].kind, TokenKind::Flag);
        assert_eq!(spans[2].kind, TokenKind::String);
        assert_eq!(spans[3].kind, TokenKind::Plain);
    }

    #[test]
    fn variables_and_comments() {
        let mut cache = TokenCache::new();
        let spans = tokenize(&mut cache, "echo $PATH # comment");
        assert_eq!(spans[0].kind, TokenKind::Command);
        assert_eq!(spans[1].kind, TokenKind::Variable);
        assert_eq!(spans[2].kind, TokenKind::Comment);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn cache_hit_no_reparse() {
        let mut cache = TokenCache::new();
        let cmd = "echo test";
        let _ = tokenize(&mut cache, cmd);
        let h = match fnv1a(cmd.as_bytes()) { 0 => 1, o => o };
        let slot = (h as usize) & (2048 - 1);
        assert_eq!(cache.slots[slot].0, h);
        assert!(!cache.slots[slot].1.is_empty());

        // Mutate the cached span to Comment
        cache.slots[slot].1[0].kind = TokenKind::Comment;

        // Calling tokenize again must return the mutated span from cache, not re-parse into Command
        let spans = tokenize(&mut cache, cmd);
        assert_eq!(spans[0].kind, TokenKind::Comment, "must return cached span without re-parsing");
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn cache_eviction_different_hash_same_slot() {
        let mut cache = PreviewCache::new();
        // PreviewCache has 128 slots.
        // Find two strings that map to the same slot (h & 127) but have different hashes.
        let mut s1 = String::new();
        let mut s2 = String::new();
        let mut target_slot = 0;
        let mut found = false;

        for i in 0..1000 {
            let str_a = format!("cmd_a_{i}");
            let h_a = match fnv1a(str_a.as_bytes()) { 0 => 1, o => o };
            let slot_a = (h_a as usize) & (128 - 1);

            for j in (i + 1)..1000 {
                let str_b = format!("cmd_b_{j}");
                let h_b = match fnv1a(str_b.as_bytes()) { 0 => 1, o => o };
                let slot_b = (h_b as usize) & (128 - 1);

                if slot_a == slot_b && h_a != h_b {
                    s1 = str_a;
                    s2 = str_b;
                    target_slot = slot_a;
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }

        assert!(found, "should find two strings mapping to same slot");
        let h1 = match fnv1a(s1.as_bytes()) { 0 => 1, o => o };
        let h2 = match fnv1a(s2.as_bytes()) { 0 => 1, o => o };

        let _ = preview(&mut cache, &s1);
        assert_eq!(cache.slots[target_slot].0, h1);

        // Evict slot with s2
        let _ = preview(&mut cache, &s2);
        assert_eq!(cache.slots[target_slot].0, h2);
    }

    #[test]
    fn style_for_all_tokens() {
        let kinds = [
            TokenKind::Command,
            TokenKind::Flag,
            TokenKind::String,
            TokenKind::Variable,
            TokenKind::Operator,
            TokenKind::Comment,
            TokenKind::Plain,
        ];

        for &kind in &kinds {
            let _dark_style = style_for(kind, Theme::Dark);
            let _light_style = style_for(kind, Theme::Light);
        }
    }
}
