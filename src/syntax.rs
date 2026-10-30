//! Tree-sitter bash syntax highlighting and token cache.
//! Direct-mapped cache for zero-realloc AST tokenization.

use ratatui::style::{Color, Modifier, Style};
use std::cell::RefCell;
use tree_sitter::{Node, Parser};

use crate::arena::{Theme, fnv1a};

// ── Types ────────────────────────────────────────────────────────

/// A colored span within a command string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: u16,
    pub end: u16,
    pub kind: TokenKind,
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

// ── Generic AstCache ─────────────────────────────────────────────

/// Direct-mapped AST cache with generic capacity N (must be power of two).
#[derive(Debug)]
pub struct AstCache<const N: usize> {
    slots: Box<[(u64, Vec<Span>); N]>,
}

impl<const N: usize> AstCache<N> {
    #[must_use]
    pub fn new() -> Self {
        let v: Vec<(u64, Vec<Span>)> = (0..N).map(|_| (0, Vec::new())).collect();
        let Ok(slots) = v.into_boxed_slice().try_into() else {
            unreachable!()
        };
        Self { slots }
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn get_or_parse<'a>(&'a mut self, cmd: &str) -> &'a [Span] {
        if cmd.is_empty() {
            return &[];
        }
        let h = match fnv1a(cmd.as_bytes()) {
            0 => 1,
            other => other,
        };
        let slot = (h as usize) & (N - 1);
        if self.slots[slot].0 == h {
            return &self.slots[slot].1;
        }

        self.slots[slot].0 = h;
        self.slots[slot].1.clear();
        parse_bash(cmd, &mut self.slots[slot].1);
        &self.slots[slot].1
    }
}

impl<const N: usize> Default for AstCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

pub type TokenCache = AstCache<2048>;
pub type PreviewCache = AstCache<128>;

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

// ── AST Traversal & Mapping ──────────────────────────────────────

#[allow(clippy::cast_possible_truncation)]
fn collect_spans(node: Node<'_>, cmd: &[u8], spans: &mut Vec<Span>) {
    let (sb, eb) = (node.start_byte(), node.end_byte());
    if sb >= eb || eb > u16::MAX as usize {
        return;
    }
    let (start, end) = (sb as u16, eb as u16);

    let kind = match node.kind() {
        "command_name" => Some(TokenKind::Command),
        "comment" => Some(TokenKind::Comment),
        "string" | "raw_string" | "heredoc_body" | "heredoc_start" => Some(TokenKind::String),
        "variable_name" | "simple_expansion" | "expansion" => Some(TokenKind::Variable),
        "|" | "||" | "&&" | "&" | ";" | ";;" | ">" | ">>" | "<" | "<<" | "<<<" | ">&" | "<&"
        | "|&" | "$" => Some(TokenKind::Operator),
        "word" => Some(if cmd[sb..eb].starts_with(b"-") {
            TokenKind::Flag
        } else {
            TokenKind::Plain
        }),
        _ => None,
    };

    if let Some(kind) = kind {
        spans.push(Span { start, end, kind });
        return;
    }

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
        parser.reset();
    });
}

// ── Public API ───────────────────────────────────────────────────

/// Tokenize a command string into colored spans using tree-sitter.
#[inline]
pub fn tokenize<'a>(cache: &'a mut TokenCache, cmd: &str) -> &'a [Span] {
    cache.get_or_parse(cmd)
}

/// Produce full syntax-highlighted preview for a multi-line command.
#[inline]
pub fn preview<'a>(cache: &'a mut PreviewCache, cmd: &str) -> &'a [Span] {
    cache.get_or_parse(cmd)
}

/// Map `TokenKind` to ANSI color based on `Theme`.
#[must_use]
pub fn style_for(kind: TokenKind, theme: Theme) -> Style {
    let (c, m) = match (theme, kind) {
        (Theme::Dark, TokenKind::Command) => (Color::Indexed(109), Modifier::BOLD),
        (Theme::Dark, TokenKind::Flag) => (Color::Indexed(103), Modifier::ITALIC),
        (Theme::Dark, TokenKind::String) => (Color::Indexed(108), Modifier::ITALIC),
        (Theme::Dark, TokenKind::Variable) => (Color::Indexed(180), Modifier::empty()),
        (Theme::Dark, TokenKind::Operator) => (Color::Indexed(167), Modifier::empty()),
        (Theme::Dark, TokenKind::Comment) => (Color::Indexed(243), Modifier::ITALIC),
        (Theme::Dark, TokenKind::Plain) => (Color::Indexed(251), Modifier::empty()),
        (Theme::Light, TokenKind::Command) => (Color::Blue, Modifier::BOLD),
        (Theme::Light, TokenKind::Flag) => (Color::Yellow, Modifier::ITALIC),
        (Theme::Light, TokenKind::String) => (Color::Green, Modifier::ITALIC),
        (Theme::Light, TokenKind::Variable) => (Color::Magenta, Modifier::empty()),
        (Theme::Light, TokenKind::Operator) => (Color::Red, Modifier::empty()),
        (Theme::Light, TokenKind::Comment) => (Color::DarkGray, Modifier::ITALIC),
        (Theme::Light, TokenKind::Plain) => (Color::Black, Modifier::empty()),
        (Theme::Catppuccin, TokenKind::Command) => (Color::Rgb(137, 180, 250), Modifier::BOLD),
        (Theme::Catppuccin, TokenKind::Flag) => (Color::Rgb(245, 194, 231), Modifier::ITALIC),
        (Theme::Catppuccin, TokenKind::String) => (Color::Rgb(166, 227, 161), Modifier::ITALIC),
        (Theme::Catppuccin, TokenKind::Variable) => (Color::Rgb(249, 226, 175), Modifier::empty()),
        (Theme::Catppuccin, TokenKind::Operator) => (Color::Rgb(148, 226, 213), Modifier::empty()),
        (Theme::Catppuccin, TokenKind::Comment) => (Color::Rgb(108, 112, 134), Modifier::ITALIC),
        (Theme::Catppuccin, TokenKind::Plain) => (Color::Rgb(205, 214, 244), Modifier::empty()),
        (Theme::TokyoNight, TokenKind::Command) => (Color::Rgb(122, 162, 247), Modifier::BOLD),
        (Theme::TokyoNight, TokenKind::Flag) => (Color::Rgb(187, 154, 247), Modifier::ITALIC),
        (Theme::TokyoNight, TokenKind::String) => (Color::Rgb(158, 206, 106), Modifier::ITALIC),
        (Theme::TokyoNight, TokenKind::Variable) => (Color::Rgb(224, 175, 104), Modifier::empty()),
        (Theme::TokyoNight, TokenKind::Operator) => (Color::Rgb(247, 118, 142), Modifier::empty()),
        (Theme::TokyoNight, TokenKind::Comment) => (Color::Rgb(86, 95, 137), Modifier::ITALIC),
        (Theme::TokyoNight, TokenKind::Plain) => (Color::Rgb(192, 202, 245), Modifier::empty()),
        (Theme::Gruvbox, TokenKind::Command) => (Color::Rgb(131, 165, 152), Modifier::BOLD),
        (Theme::Gruvbox, TokenKind::Flag) => (Color::Rgb(211, 134, 155), Modifier::ITALIC),
        (Theme::Gruvbox, TokenKind::String) => (Color::Rgb(184, 187, 38), Modifier::ITALIC),
        (Theme::Gruvbox, TokenKind::Variable) => (Color::Rgb(250, 189, 47), Modifier::empty()),
        (Theme::Gruvbox, TokenKind::Operator) => (Color::Rgb(251, 73, 52), Modifier::empty()),
        (Theme::Gruvbox, TokenKind::Comment) => (Color::Rgb(146, 131, 116), Modifier::ITALIC),
        (Theme::Gruvbox, TokenKind::Plain) => (Color::Rgb(235, 219, 178), Modifier::empty()),
        (Theme::Nord, TokenKind::Command) => (Color::Rgb(129, 161, 193), Modifier::BOLD),
        (Theme::Nord, TokenKind::Flag) => (Color::Rgb(136, 192, 208), Modifier::ITALIC),
        (Theme::Nord, TokenKind::String) => (Color::Rgb(163, 190, 140), Modifier::ITALIC),
        (Theme::Nord, TokenKind::Variable) => (Color::Rgb(235, 203, 139), Modifier::empty()),
        (Theme::Nord, TokenKind::Operator) => (Color::Rgb(191, 97, 106), Modifier::empty()),
        (Theme::Nord, TokenKind::Comment) => (Color::Rgb(76, 86, 106), Modifier::ITALIC),
        (Theme::Nord, TokenKind::Plain) => (Color::Rgb(236, 239, 244), Modifier::empty()),
    };
    Style::default().fg(c).add_modifier(m)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(all(test, not(miri)))]
#[allow(clippy::expect_used, clippy::panic)]
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
        let h = match fnv1a(cmd.as_bytes()) {
            0 => 1,
            o => o,
        };
        let slot = (h as usize) & (2048 - 1);
        assert_eq!(cache.slots[slot].0, h);
        assert!(!cache.slots[slot].1.is_empty());

        // Mutate the cached span to Comment
        cache.slots[slot].1[0].kind = TokenKind::Comment;

        // Calling tokenize again must return the mutated span from cache, not re-parse into Command
        let spans = tokenize(&mut cache, cmd);
        assert_eq!(
            spans[0].kind,
            TokenKind::Comment,
            "must return cached span without re-parsing"
        );
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
            let h_a = match fnv1a(str_a.as_bytes()) {
                0 => 1,
                o => o,
            };
            let slot_a = (h_a as usize) & (128 - 1);

            for j in (i + 1)..1000 {
                let str_b = format!("cmd_b_{j}");
                let h_b = match fnv1a(str_b.as_bytes()) {
                    0 => 1,
                    o => o,
                };
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
        let h1 = match fnv1a(s1.as_bytes()) {
            0 => 1,
            o => o,
        };
        let h2 = match fnv1a(s2.as_bytes()) {
            0 => 1,
            o => o,
        };

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
