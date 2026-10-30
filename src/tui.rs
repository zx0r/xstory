//! Interactive TUI dashboard for searching and selecting shell history.
//! Uses crossterm for raw mode and ratatui for differential rendering to stderr.

use std::cell::RefCell;
use std::io::Write;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};

use crate::arena::{Arena, Config, PreviewMode, Scope, Theme};
use crate::error::Error;
use crate::search::{self, SearchResult};
use crate::syntax::{self, PreviewCache, TokenCache};

// ── Types ────────────────────────────────────────────────────────

/// Terminal exit outcome — determines process exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Execute(String), // exit 0
    Edit(String),    // exit 2
    Cancel,          // exit 130
}

/// Focused panel in the TUI dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPanel {
    Query,
    History,
    Preview,
}

impl FocusedPanel {
    #[must_use]
    pub const fn up(self) -> Self {
        match self {
            Self::Query => Self::Preview,
            Self::Preview | Self::History => Self::History,
        }
    }

    #[must_use]
    pub const fn down(self) -> Self {
        match self {
            Self::History => Self::Preview,
            Self::Preview | Self::Query => Self::Query,
        }
    }
}

/// The entire TUI state. Owned by `run_tui`, mutated in-place.
#[derive(Debug)]
pub struct Dashboard<'a> {
    // --- input ---
    pub query: String,
    pub cursor: usize,

    // --- focus ---
    pub focus: FocusedPanel,

    // --- search ---
    pub scope: Scope,
    pub result: SearchResult<'a>,
    pub dedup_total: usize,

    // --- navigation ---
    pub selected: usize,
    pub viewport_offset: usize,
    pub preview_scroll: usize,
    pub preview_mode: PreviewMode,

    // --- display ---
    pub theme: Theme,
    pub width: u16,
    pub height: u16,

    // --- frame buffer ---
    pub dirty: bool,
}

impl<'a> Dashboard<'a> {
    /// Create a new Dashboard state with initial values.
    #[must_use]
    pub fn new(
        theme: Theme,
        width: u16,
        height: u16,
        initial_q: Option<&str>,
        arena: &'a Arena,
    ) -> Self {
        let query = initial_q.unwrap_or("").to_owned();
        let cursor = query.chars().count();
        let dedup_total = arena.scope_indices(Scope::Global).len();
        let result = search::search(arena, &query, Scope::Global);
        Self {
            query,
            cursor,
            focus: FocusedPanel::Query,
            scope: Scope::Global,
            result,
            dedup_total,
            selected: 0,
            viewport_offset: 0,
            preview_scroll: 0,
            preview_mode: PreviewMode::default(),
            theme,
            width,
            height,
            dirty: true,
        }
    }

    #[inline]
    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.query
            .char_indices()
            .nth(char_idx)
            .map_or(self.query.len(), |(i, _)| i)
    }

    /// Insert a character at current cursor position (UTF-8 safe).
    pub fn insert_char(&mut self, c: char) {
        let b = self.char_to_byte(self.cursor);
        self.query.insert(b, c);
        self.cursor += 1;
    }

    /// Delete character immediately before the cursor (UTF-8 safe).
    pub fn delete_char_before(&mut self) {
        if self.cursor > 0 {
            let (s, e) = (
                self.char_to_byte(self.cursor - 1),
                self.char_to_byte(self.cursor),
            );
            self.query.drain(s..e);
            self.cursor -= 1;
        }
    }

    /// Delete from start of query up to cursor (Ctrl-U).
    pub fn delete_to_start(&mut self) {
        if self.cursor > 0 {
            let e = self.char_to_byte(self.cursor);
            self.query.drain(..e);
            self.cursor = 0;
        }
    }

    /// Delete previous word before cursor (Ctrl-W).
    pub fn delete_word_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.char_to_byte(self.cursor);
        let prefix = &self.query[..end];
        let trimmed = prefix.trim_end();
        let start = trimmed.rfind(char::is_whitespace).map_or(0, |i| {
            i + prefix[i..].chars().next().map_or(1, char::len_utf8)
        });
        self.cursor -= prefix[start..end].chars().count();
        self.query.drain(start..end);
    }

    /// Trigger search query change, resetting selection and scroll.
    pub fn on_query_mutated(&mut self, arena: &'a Arena) {
        self.focus = FocusedPanel::Query;
        self.result = search::search(arena, &self.query, self.scope);
        self.selected = 0;
        self.preview_scroll = 0;
        self.dirty = true;
    }

    pub const fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.query.chars().count());
    }
    pub const fn move_cursor_home(&mut self) {
        self.cursor = 0;
    }
    pub fn move_cursor_end(&mut self) {
        self.cursor = self.query.chars().count();
    }

    /// Move selection index up or down with saturation.
    pub fn move_selection(&mut self, delta: isize) {
        if self.result.indices.is_empty() {
            self.selected = 0;
            return;
        }
        let max_idx = self.result.indices.len().saturating_sub(1);
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            #[allow(clippy::cast_sign_loss)]
            self.selected.saturating_add(delta as usize).min(max_idx)
        };
        self.preview_scroll = 0;
    }

    /// Toggle between Global and Local search scopes.
    pub const fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::Global => Scope::Local,
            Scope::Local => Scope::Global,
        };
        self.preview_scroll = 0;
    }

    /// Update viewport offset taking `scroll_off` cushion into account.
    pub fn update_viewport(&mut self, visible_rows: usize, scroll_off: u16) {
        let total = self.result.indices.len();
        if visible_rows == 0 || total <= visible_rows {
            self.viewport_offset = 0;
            return;
        }
        let m = (scroll_off as usize).min(visible_rows.saturating_sub(1) / 2);
        if self.selected < self.viewport_offset + m {
            self.viewport_offset = self.selected.saturating_sub(m);
        } else if self.selected + m >= self.viewport_offset + visible_rows {
            self.viewport_offset = (self.selected + m + 1).saturating_sub(visible_rows);
        }
        if self.viewport_offset + visible_rows > total {
            self.viewport_offset = total.saturating_sub(visible_rows);
        }
    }

    /// Scroll preview pane up or down.
    pub fn scroll_preview(&mut self, delta: isize, max_lines: usize) {
        self.preview_scroll = if delta < 0 {
            self.preview_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            #[allow(clippy::cast_sign_loss)]
            self.preview_scroll
                .saturating_add(delta as usize)
                .min(max_lines.saturating_sub(1))
        };
    }

    pub const fn resize(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
    }

    /// Get command string for current selection if present.
    #[must_use]
    pub fn selected_command(&self, arena: &Arena) -> Option<String> {
        let &idx = self.result.indices.get(self.selected)?;
        Some(arena.cmd_str(idx).to_owned())
    }

    /// Zero-copy reference to the selected command string (used in hot paths).
    #[must_use]
    pub fn selected_cmd_str<'b>(&self, arena: &'b Arena) -> Option<&'b str> {
        let &idx = self.result.indices.get(self.selected)?;
        Some(arena.cmd_str(idx))
    }

    /// Calculate current preview height taking preview mode and selection into account.
    #[must_use]
    pub fn preview_height(&self, arena: &Arena, area_height: u16, area_width: u16) -> u16 {
        match self.preview_mode {
            PreviewMode::Fixed => {
                if area_height >= 18 {
                    PREVIEW_PANEL_HEIGHT
                } else {
                    PREVIEW_PANEL_HEIGHT.min(area_height.saturating_sub(8))
                }
            }
            PreviewMode::Dynamic => {
                let max_preview_h = (area_height / 2)
                    .clamp(6, 20)
                    .min(area_height.saturating_sub(8));
                calc_preview_height(self.selected_cmd_str(arena), area_width, max_preview_h)
            }
        }
    }
}

// ── Theme Detection ──────────────────────────────────────────────

/// Detect terminal theme using 4-tier cascade.
#[must_use]
pub fn detect_theme() -> Theme {
    if let Ok(val) = std::env::var("COLORFGBG")
        && let Some(bg_str) = val.rsplit(';').next()
        && let Ok(bg_num) = bg_str.trim().parse::<u8>()
    {
        return match bg_num {
            0..=6 | 8 => Theme::Dark,
            _ => Theme::Light,
        };
    }
    if let Ok(profile) = std::env::var("ITERM_PROFILE") {
        let lower = profile.to_lowercase();
        if lower.contains("light") || lower.contains("white") {
            return Theme::Light;
        }
        if lower.contains("dark") || lower.contains("black") {
            return Theme::Dark;
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
    {
        return if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "Dark" {
            Theme::Dark
        } else {
            Theme::Light
        };
    }
    Theme::Dark
}

// ── Terminal Setup & Teardown ────────────────────────────────────

fn setup_terminal()
-> Result<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>, Error> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stderr = std::io::stderr();
    crossterm::execute!(
        stderr,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture
    )?;
    Ok(ratatui::Terminal::new(
        ratatui::backend::CrosstermBackend::new(stderr),
    )?)
}

/// Reset terminal state back to normal mode (crossterm).
pub fn reset_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut stderr = std::io::stderr();
    let _ = crossterm::execute!(
        stderr,
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = stderr.flush();
}

fn restore_terminal(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>,
) {
    reset_terminal();
    let _ = terminal.backend_mut().flush();
}

// ── Formatting Helpers ───────────────────────────────────────────

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[must_use]
pub fn format_timestamp(timestamp_ms: i64) -> String {
    if timestamp_ms <= 0 {
        return "--:--".to_owned();
    }
    let (secs, day_sec) = (timestamp_ms / 1000, (timestamp_ms / 1000) % 86400);
    let (hour, min) = (day_sec / 3600, (day_sec % 3600) / 60);

    let z = (secs / 86400) + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 2 } else { mp - 10 } as usize;
    let month = MONTHS.get(m).copied().unwrap_or("???");
    format!("{d:02} {month} {hour:02}:{min:02}")
}

fn abbreviate_path(path: &str) -> String {
    if path.is_empty() || path == "?" {
        return String::new();
    }
    if let Ok(home) = std::env::var("HOME") {
        if path == home {
            return "~".to_owned();
        }
        if let Some(stripped) = path.strip_prefix(&home)
            && stripped.starts_with('/')
        {
            return format!("~{stripped}");
        }
    }
    path.to_owned()
}

fn fit_path(path: &str, max_len: usize) -> String {
    let count = path.chars().count();
    if count <= max_len || max_len < 5 {
        return path.to_owned();
    }
    let keep_front = (max_len.saturating_sub(1)) / 3;
    let keep_back = max_len.saturating_sub(1 + keep_front);
    let front_end = path
        .char_indices()
        .nth(keep_front)
        .map_or(path.len(), |(i, _)| i);
    let back_start = path
        .char_indices()
        .nth(count.saturating_sub(keep_back))
        .map_or(0, |(i, _)| i);
    format!("{}…{}", &path[..front_end], &path[back_start..])
}

fn format_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in s.bytes().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

// ── Unified Master Layout & Box Model ────────────────────────────

pub const TOP_BREATHING_ROOM: u16 = 1;
pub const ACTION_BAR_HEIGHT: u16 = 1;
pub const PREVIEW_PANEL_HEIGHT: u16 = 10;
pub const QUERY_BAR_HEIGHT: u16 = 3;
pub const HORIZONTAL_MARGIN: u16 = 1;
pub const PANEL_PADDING_HORIZONTAL: u16 = 1;

/// Master layout chunks derived from a single vertical split hierarchy.
#[derive(Debug, Clone, Copy)]
pub struct MasterLayout {
    pub ceiling: Rect,
    pub actions: Rect,
    pub history: Rect,
    pub preview: Rect,
    pub query: Rect,
}

/// Count wrapped visual rows for text inside a bounded width.
#[must_use]
pub fn count_wrapped_rows(text: &str, line_width: u16) -> usize {
    let width = line_width.max(1) as usize;
    let mut rows = 0;
    for line in text.split('\n') {
        let w = unicode_width::UnicodeWidthStr::width(line);
        rows += if w > width { w.div_ceil(width) } else { 1 };
    }
    rows.max(1)
}

/// Compute dynamic preview height based on command line count and terminal width (OTS-1 standard).
#[must_use]
pub fn calc_preview_height(command: Option<&str>, width: u16, max_height: u16) -> u16 {
    if max_height <= 2 {
        return max_height;
    }
    match command {
        Some(cmd) if !cmd.trim().is_empty() => {
            let inner_width = width.saturating_sub(6).max(1);
            let total_lines =
                u16::try_from(count_wrapped_rows(cmd, inner_width)).unwrap_or(u16::MAX);
            let max_content = max_height.saturating_sub(2);
            total_lines.clamp(1, max_content) + 2
        }
        _ => 3.min(max_height),
    }
}

/// Compute master layout tree given preview panel height.
#[must_use]
pub fn compute_master_layout(area: Rect, preview_height: u16) -> MasterLayout {
    let eff_preview_height = if preview_height <= 1 {
        0
    } else {
        preview_height
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(HORIZONTAL_MARGIN)
        .constraints([
            Constraint::Length(TOP_BREATHING_ROOM),
            Constraint::Length(ACTION_BAR_HEIGHT),
            Constraint::Min(3),
            Constraint::Length(eff_preview_height),
            Constraint::Length(QUERY_BAR_HEIGHT),
        ])
        .split(area);

    MasterLayout {
        ceiling: chunks[0],
        actions: chunks[1],
        history: chunks[2],
        preview: chunks[3],
        query: chunks[4],
    }
}

/// Get border color for a panel based on its focus state.
#[must_use]
pub const fn panel_border_color(focused: bool) -> Color {
    if focused {
        Color::Indexed(109)
    } else {
        Color::Indexed(241)
    }
}

/// Create a unified panel block with standard borders, rounded corners, and zero padding.
#[must_use]
pub fn create_panel_block(focused: bool) -> Block<'static> {
    let fg = panel_border_color(focused);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(fg))
        .padding(Padding::ZERO)
}

impl MasterLayout {
    #[must_use]
    pub fn history_inner(self) -> Rect {
        create_panel_block(false)
            .padding(Padding::new(0, 1, 0, 0))
            .inner(self.history)
    }
    #[must_use]
    pub fn preview_inner(self) -> Rect {
        create_panel_block(false)
            .padding(Padding::horizontal(1))
            .inner(self.preview)
    }
    #[must_use]
    pub fn query_inner(self) -> Rect {
        create_panel_block(false)
            .padding(Padding::horizontal(1))
            .inner(self.query)
    }
}

#[inline]
const fn in_rect(col: u16, row: u16, rect: Rect) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn bracketed<'a>(
    inner: impl Into<std::borrow::Cow<'a, str>>,
    border_color: Color,
) -> Vec<Span<'a>> {
    let b = Style::default().fg(border_color);
    let val = Style::default().fg(Color::Indexed(243));
    vec![
        Span::styled("[", b),
        Span::styled(inner, val),
        Span::styled("]", b),
    ]
}

/// Static whitespace buffer in `.rodata` for zero-allocation row padding.
const SPACES: &str = concat!(
    "                                                                ", // 64
    "                                                                ", // 128
    "                                                                ", // 192
    "                                                                ", // 256
    "                                                                ", // 320
    "                                                                ", // 384
    "                                                                ", // 448
    "                                                                ", // 512
);

thread_local! {
    static SCRATCH_OFFSETS: RefCell<Vec<(usize, char)>> = const { RefCell::new(Vec::new()) };
    static SCRATCH_STYLES: RefCell<Vec<Style>> = const { RefCell::new(Vec::new()) };
    static SCRATCH_MATCHES: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    static SCRATCH_QCHARS: RefCell<Vec<char>> = const { RefCell::new(Vec::new()) };
}

fn split_spans_into_lines<'a>(spans: Vec<Span<'a>>) -> Vec<Line<'a>> {
    let (mut lines, mut cur) = (Vec::new(), Vec::new());
    for span in spans {
        let text: &'a str = match span.content {
            std::borrow::Cow::Borrowed(s) => s,
            std::borrow::Cow::Owned(_) => "",
        };
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Line::from(std::mem::take(&mut cur)));
            }
            if !part.is_empty() {
                cur.push(Span::styled(part, span.style));
            }
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

// ── Frame Rendering ──────────────────────────────────────────────

fn extract_query_term(query: &str) -> &str {
    let t = query.trim();
    t.strip_prefix('^')
        .or_else(|| t.strip_prefix('\''))
        .or_else(|| t.strip_suffix('$'))
        .unwrap_or(t)
}

/// Zero-Destructive AST syntax highlighter with character-level query match overlay.
/// Fast zero-alloc path on ASCII commands; reuses thread-local scratch vectors on UTF-8.
const MATCH_STYLE: Style = Style::new()
    .fg(Color::Indexed(215))
    .add_modifier(Modifier::BOLD);

/// Apply fuzzy subsequence match overlay for ASCII-only commands (byte-indexed).
fn apply_ascii_match_overlay(query: &str, bytes: &[u8], styles: &mut [Style]) {
    let term = extract_query_term(query);
    if term.is_empty() || !term.is_ascii() {
        return;
    }
    let t_bytes = term.as_bytes();
    SCRATCH_MATCHES.with_borrow_mut(|matches| {
        matches.clear();
        let mut t_idx = 0;
        for (b_idx, &b) in bytes.iter().enumerate() {
            if t_idx < t_bytes.len() && b.eq_ignore_ascii_case(&t_bytes[t_idx]) {
                matches.push(b_idx);
                t_idx += 1;
            }
        }
        if t_idx == t_bytes.len() {
            for &idx in matches.iter() {
                styles[idx] = MATCH_STYLE;
            }
        }
    });
}

/// Apply fuzzy subsequence match overlay for UTF-8 commands (char-indexed).
fn apply_utf8_match_overlay(
    query: &str,
    char_offsets: &[(usize, char)],
    char_styles: &mut [Style],
) {
    let term = extract_query_term(query);
    if term.is_empty() {
        return;
    }
    SCRATCH_QCHARS.with_borrow_mut(|q_chars| {
        q_chars.clear();
        q_chars.extend(term.chars());
        SCRATCH_MATCHES.with_borrow_mut(|matches| {
            matches.clear();
            let mut q_idx = 0;
            for (t_idx, &(_, ch)) in char_offsets.iter().enumerate() {
                if q_idx < q_chars.len()
                    && (ch.eq_ignore_ascii_case(&q_chars[q_idx])
                        || ch.to_lowercase().eq(q_chars[q_idx].to_lowercase()))
                {
                    matches.push(t_idx);
                    q_idx += 1;
                }
            }
            if q_idx == q_chars.len() {
                for &idx in matches.iter() {
                    if idx < char_styles.len() {
                        char_styles[idx] = MATCH_STYLE;
                    }
                }
            }
        });
    });
}

#[allow(clippy::too_many_lines)]
fn build_highlighted_spans<'a>(
    cmd: &'a str,
    spans: &[syntax::Span],
    theme: Theme,
    query: Option<&str>,
) -> Vec<Span<'a>> {
    if cmd.is_empty() {
        return Vec::new();
    }

    let default_style = syntax::style_for(syntax::TokenKind::Plain, theme);

    // Fast zero-alloc path when no query match overlay is requested
    if query.is_none() {
        if spans.is_empty() {
            return vec![Span::styled(cmd, default_style)];
        }
        let mut out = Vec::with_capacity(spans.len() * 2 + 1);
        let mut cursor = 0;
        for span in spans {
            let s = (span.start as usize).min(cmd.len());
            let e = (span.end as usize).min(cmd.len());
            if cursor < s {
                out.push(Span::styled(&cmd[cursor..s], default_style));
            }
            if s < e {
                out.push(Span::styled(
                    &cmd[s..e],
                    syntax::style_for(span.kind, theme),
                ));
            }
            cursor = cursor.max(e);
        }
        if cursor < cmd.len() {
            out.push(Span::styled(&cmd[cursor..], default_style));
        }
        return out;
    }

    if cmd.is_ascii() {
        let bytes = cmd.as_bytes();
        let len = bytes.len();

        return SCRATCH_STYLES.with_borrow_mut(|styles| {
            styles.clear();
            styles.resize(len, default_style);

            // 1. AST syntax token layer (direct byte offsets)
            for span in spans {
                let s = (span.start as usize).min(len);
                let e = (span.end as usize).min(len);
                if s < e {
                    styles[s..e].fill(syntax::style_for(span.kind, theme));
                }
            }

            // 2. Query match overlay (Zero-Destructive Subsequence Overlay)
            if let Some(q) = query {
                apply_ascii_match_overlay(q, bytes, styles);
            }

            // 3. Span coalescing (group adjacent characters of identical style into zero-copy slices)
            let mut out = Vec::with_capacity(spans.len() + 4);
            let mut i = 0;
            while i < len {
                let style = styles[i];
                let mut j = i + 1;
                while j < len && styles[j] == style {
                    j += 1;
                }
                out.push(Span::styled(&cmd[i..j], style));
                i = j;
            }
            out
        });
    }

    // General UTF-8 fallback using reusable scratch vectors (zero heap reallocations)
    SCRATCH_OFFSETS.with_borrow_mut(|char_offsets| {
        char_offsets.clear();
        char_offsets.extend(cmd.char_indices());
        let char_count = char_offsets.len();
        if char_count == 0 {
            return Vec::new();
        }

        SCRATCH_STYLES.with_borrow_mut(|char_styles| {
            char_styles.clear();
            char_styles.resize(char_count, default_style);

            // 1. AST syntax token layer
            for span in spans {
                let (s_b, e_b) = (span.start as usize, span.end as usize);
                let s_c = char_offsets
                    .binary_search_by_key(&s_b, |&(b, _)| b)
                    .unwrap_or_else(|i| i)
                    .min(char_count);
                let e_c = char_offsets
                    .binary_search_by_key(&e_b, |&(b, _)| b)
                    .unwrap_or_else(|i| i)
                    .min(char_count);
                char_styles[s_c..e_c].fill(syntax::style_for(span.kind, theme));
            }

            // 2. Query match overlay
            if let Some(q) = query {
                apply_utf8_match_overlay(q, char_offsets, char_styles);
            }

            // 3. Span coalescing
            let mut out = Vec::with_capacity(spans.len() + 4);
            let mut i = 0;
            while i < char_count {
                let style = char_styles[i];
                let mut j = i + 1;
                while j < char_count && char_styles[j] == style {
                    j += 1;
                }
                let b_start = char_offsets[i].0;
                let b_end = if j < char_count {
                    char_offsets[j].0
                } else {
                    cmd.len()
                };
                if let Some(slice) = cmd.get(b_start..b_end) {
                    out.push(Span::styled(slice, style));
                }
                i = j;
            }
            out
        })
    })
}

/// Render a single TUI frame to the ratatui Frame.
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn render_frame(
    f: &mut Frame<'_>,
    dash: &Dashboard<'_>,
    arena: &Arena,
    token_cache: &mut TokenCache,
    preview_cache: &mut PreviewCache,
    layout: MasterLayout,
) {
    let area = f.area();
    if area.width < 20 || area.height < 5 {
        f.render_widget(Paragraph::new("Terminal too small"), area);
        return;
    }

    // 1. Action Buttons
    let items = [
        ("[Esc]", " Exit"),
        ("[^R]", " Scope"),
        ("[↵]", " Run"),
        ("[Tab]", " Edit"),
    ];
    let header_spans: Vec<Span<'_>> = items
        .iter()
        .enumerate()
        .flat_map(|(i, (k, d))| {
            let mut v = Vec::new();
            if i > 0 {
                v.push(Span::raw("   "));
            }
            v.push(Span::styled(*k, Style::default().fg(Color::Indexed(241))));
            v.push(Span::styled(
                *d,
                Style::default()
                    .fg(Color::Indexed(241))
                    .add_modifier(Modifier::ITALIC),
            ));
            v
        })
        .collect();
    f.render_widget(
        Line::from(header_spans).alignment(Alignment::Center),
        layout.actions,
    );

    // 2. History List
    let history_focus = dash.focus == FocusedPanel::History;
    let scope_char = match dash.scope {
        Scope::Local => "L",
        Scope::Global => "G",
    };
    let total = dash.result.indices.len();
    let current = if total == 0 {
        0
    } else {
        dash.selected.min(total.saturating_sub(1)) + 1
    };
    let count_str = format!("{}/{}", format_thousands(current), format_thousands(total));

    let h_border = panel_border_color(history_focus);
    let history_block = create_panel_block(history_focus)
        .padding(Padding::new(0, 1, 0, 0))
        .title_top(Line::from(bracketed(scope_char, h_border)).alignment(Alignment::Left))
        .title_top(Line::from(bracketed(count_str, h_border)).alignment(Alignment::Right));

    let h_inner = history_block.inner(layout.history);
    let (h_h, h_w) = (h_inner.height as usize, h_inner.width as usize);
    let sel_bg = match dash.theme {
        Theme::Dark => Color::Indexed(237),
        Theme::Light => Color::Gray,
    };

    let history_lines: Vec<Line<'_>> = (0..h_h)
        .map(|row_idx| {
            let from_bottom = h_h.saturating_sub(1).saturating_sub(row_idx);
            let item_idx = dash.viewport_offset + from_bottom;
            if item_idx >= dash.result.indices.len() {
                return Line::raw("");
            }
            let cmd = arena.cmd_str(dash.result.indices[item_idx]);
            let is_selected = item_idx == dash.selected;
            let mut spans = Vec::new();
            spans.push(if is_selected {
                Span::styled(
                    "> ",
                    Style::default()
                        .fg(Color::Indexed(215))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            });
            spans.extend(build_highlighted_spans(
                cmd,
                syntax::tokenize(token_cache, cmd),
                dash.theme,
                Some(&dash.query),
            ));
            let used_width = 2 + cmd.chars().count();
            let needed = h_w.saturating_sub(used_width);
            if needed > 0 {
                let pad = needed.min(SPACES.len());
                spans.push(Span::raw(&SPACES[..pad]));
            }
            if is_selected {
                for s in &mut spans {
                    s.style = s.style.bg(sel_bg);
                }
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(
        Paragraph::new(history_lines).block(history_block),
        layout.history,
    );

    // 3. Preview Box
    let preview_focus = dash.focus == FocusedPanel::Preview;
    let (time_str, path_str) = dash.result.indices.get(dash.selected).map_or_else(
        || ("--:--".to_owned(), String::new()),
        |&idx| {
            (
                format_timestamp(arena.timestamps[idx as usize]),
                abbreviate_path(arena.cwd_str(idx)),
            )
        },
    );
    let p_border = panel_border_color(preview_focus);
    let mut preview_block = create_panel_block(preview_focus)
        .padding(Padding::horizontal(1))
        .title_top(Line::from(bracketed(time_str, p_border)).alignment(Alignment::Left));

    if !path_str.is_empty() {
        let p_inner_w = preview_block.inner(layout.preview).width as usize;
        let avail = p_inner_w.saturating_sub(18);
        if avail >= 4 {
            let fitted = fit_path(&path_str, avail);
            preview_block = preview_block
                .title_top(Line::from(bracketed(fitted, p_border)).alignment(Alignment::Right));
        }
    }

    let preview_lines = dash
        .result
        .indices
        .get(dash.selected)
        .map_or_else(Vec::new, |&idx| {
            let cmd = arena.cmd_str(idx);
            let spans = syntax::preview(preview_cache, cmd);
            split_spans_into_lines(build_highlighted_spans(cmd, spans, dash.theme, None))
        });

    let p_inner = preview_block.inner(layout.preview);
    let visual_rows = dash.result.indices.get(dash.selected).map_or(0, |&idx| {
        count_wrapped_rows(arena.cmd_str(idx), p_inner.width)
    });

    #[allow(clippy::cast_possible_truncation)]
    let max_scroll = (visual_rows as u16).saturating_sub(p_inner.height);
    #[allow(clippy::cast_possible_truncation)]
    let p_scroll = (dash.preview_scroll as u16).min(max_scroll);

    if visual_rows > p_inner.height as usize {
        let scroll_str = format!("{}/{} • ↓/↑", p_scroll + 1, visual_rows);
        preview_block = preview_block
            .title_bottom(Line::from(bracketed(scroll_str, p_border)).alignment(Alignment::Right));
    }
    f.render_widget(
        Paragraph::new(preview_lines)
            .block(preview_block)
            .wrap(Wrap { trim: false })
            .scroll((p_scroll, 0)),
        layout.preview,
    );

    // 4. Search Query Box
    let query_focus = dash.focus == FocusedPanel::Query;
    let query_block = create_panel_block(query_focus).padding(Padding::horizontal(1));
    let q_inner = query_block.inner(layout.query);
    let line = if dash.query.is_empty() {
        Line::from(vec![Span::styled(
            "",
            Style::default().fg(Color::Indexed(241)),
        )])
    } else {
        Line::from(vec![Span::raw(&dash.query)])
    };
    f.render_widget(Paragraph::new(line).block(query_block), layout.query);

    #[allow(clippy::cast_possible_truncation)]
    let cursor_offset = if dash.query.is_empty() {
        2
    } else {
        dash.cursor as u16
    };
    let cursor_x = (q_inner.x + cursor_offset).min(q_inner.x + q_inner.width.saturating_sub(1));
    f.set_cursor_position((cursor_x, q_inner.y));
}

// ── Public Event Loop ────────────────────────────────────────────

/// Run the interactive TUI. Borrows Arena for the entire session.
/// Returns Outcome determining the process exit code.
///
/// # Errors
/// Returns `Error::Io` on terminal manipulation or event read failure.
#[allow(clippy::too_many_lines)]
pub fn run_tui(
    arena: &Arena,
    config: &Config,
    initial_query: Option<&str>,
) -> Result<Outcome, Error> {
    let theme = detect_theme();
    let (w, h) = crossterm::terminal::size()?;
    let mut dash = Dashboard::new(theme, w, h, initial_query, arena);
    dash.preview_mode = config.preview_mode;

    let mut token_cache = TokenCache::new();
    let mut preview_cache = PreviewCache::new();
    let mut terminal = setup_terminal()?;

    let outcome = loop {
        if dash.dirty {
            let p_h = dash.preview_height(arena, dash.height, dash.width);
            let layout = compute_master_layout(Rect::new(0, 0, dash.width, dash.height), p_h);
            let visible_rows = layout.history_inner().height as usize;
            dash.update_viewport(visible_rows, config.scroll_off);

            terminal.draw(|f| {
                render_frame(
                    f,
                    &dash,
                    arena,
                    &mut token_cache,
                    &mut preview_cache,
                    layout,
                );
            })?;
            dash.dirty = false;
        }

        match crossterm::event::read()? {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (modifiers, code) {
                (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                    dash.insert_char(c);
                    dash.on_query_mutated(arena);
                }
                (KeyModifiers::NONE, KeyCode::Backspace) => {
                    dash.delete_char_before();
                    dash.on_query_mutated(arena);
                }
                (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                    dash.delete_to_start();
                    dash.on_query_mutated(arena);
                }
                (KeyModifiers::CONTROL, KeyCode::Char('r')) => {
                    dash.toggle_scope();
                    dash.dedup_total = arena.scope_indices(dash.scope).len();
                    dash.on_query_mutated(arena);
                }
                (KeyModifiers::NONE, KeyCode::Left) => {
                    dash.focus = FocusedPanel::Query;
                    dash.move_cursor_left();
                    dash.dirty = true;
                }
                (KeyModifiers::NONE, KeyCode::Right) => {
                    dash.focus = FocusedPanel::Query;
                    dash.move_cursor_right();
                    dash.dirty = true;
                }
                (KeyModifiers::NONE, KeyCode::Up) => {
                    if dash.focus == FocusedPanel::Preview {
                        dash.scroll_preview(-1, 1000);
                    } else {
                        dash.focus = FocusedPanel::History;
                        dash.move_selection(1);
                    }
                    dash.dirty = true;
                }
                (KeyModifiers::NONE, KeyCode::Down) => {
                    if dash.focus == FocusedPanel::Preview {
                        dash.scroll_preview(1, 1000);
                    } else {
                        dash.focus = FocusedPanel::History;
                        dash.move_selection(-1);
                    }
                    dash.dirty = true;
                }
                (KeyModifiers::SHIFT, KeyCode::Up) => {
                    dash.focus = dash.focus.up();
                    dash.dirty = true;
                }
                (KeyModifiers::SHIFT, KeyCode::Down) => {
                    dash.focus = dash.focus.down();
                    dash.dirty = true;
                }
                (KeyModifiers::CONTROL, KeyCode::Char('c'))
                | (KeyModifiers::NONE, KeyCode::Esc) => break Outcome::Cancel,
                (KeyModifiers::NONE, KeyCode::Enter) => {
                    let cmd = dash.selected_command(arena).or_else(|| {
                        let q = dash.query.trim();
                        (!q.is_empty()).then(|| dash.query.clone())
                    });
                    break cmd.map_or(Outcome::Cancel, Outcome::Execute);
                }
                (KeyModifiers::NONE, KeyCode::Tab) => {
                    let cmd = dash
                        .selected_command(arena)
                        .unwrap_or_else(|| dash.query.clone());
                    break if cmd.trim().is_empty() {
                        Outcome::Cancel
                    } else {
                        Outcome::Edit(cmd)
                    };
                }
                _ => {}
            },

            Event::Mouse(mouse) => {
                let p_h = dash.preview_height(arena, dash.height, dash.width);
                let layout = compute_master_layout(Rect::new(0, 0, dash.width, dash.height), p_h);
                let is_over_preview = in_rect(mouse.column, mouse.row, layout.preview);

                match mouse.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let delta = if mouse.kind == MouseEventKind::ScrollUp {
                            1
                        } else {
                            -1
                        };
                        if is_over_preview {
                            dash.focus = FocusedPanel::Preview;
                            dash.scroll_preview(-delta, 1000);
                        } else {
                            dash.focus = FocusedPanel::History;
                            dash.move_selection(delta);
                        }
                        dash.dirty = true;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let history_inner = layout.history_inner();
                        if in_rect(mouse.column, mouse.row, history_inner) {
                            let row_offset = (mouse.row - history_inner.y) as usize;
                            let from_bottom = (history_inner.height as usize)
                                .saturating_sub(1)
                                .saturating_sub(row_offset);
                            let clicked_idx = dash.viewport_offset + from_bottom;
                            if clicked_idx < dash.result.indices.len() {
                                dash.focus = FocusedPanel::History;
                                dash.selected = clicked_idx;
                                dash.preview_scroll = 0;
                                dash.dirty = true;
                            }
                        } else if is_over_preview {
                            dash.focus = FocusedPanel::Preview;
                            dash.dirty = true;
                        } else if in_rect(mouse.column, mouse.row, layout.query_inner()) {
                            dash.focus = FocusedPanel::Query;
                            dash.dirty = true;
                        }
                    }
                    _ => {}
                }
            }

            Event::Resize(w, h) => {
                dash.resize(w, h);
                dash.dirty = true;
            }
            _ => {}
        }
    };

    restore_terminal(&mut terminal);
    Ok(outcome)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_arena(lines: &[&str]) -> (tempfile::TempDir, Arena) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let mut f = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        let arena = Arena::load(&path, Some("/home")).expect("load");
        (dir, arena)
    }

    #[test]
    fn viewport_scrolloff_cushion() {
        let (_dir, arena) = make_test_arena(&[
            "1000\t50\t0\t/home\tcmd_01",
            "2000\t50\t0\t/home\tcmd_02",
            "3000\t50\t0\t/home\tcmd_03",
            "4000\t50\t0\t/home\tcmd_04",
            "5000\t50\t0\t/home\tcmd_05",
            "6000\t50\t0\t/home\tcmd_06",
            "7000\t50\t0\t/home\tcmd_07",
            "8000\t50\t0\t/home\tcmd_08",
            "9000\t50\t0\t/home\tcmd_09",
            "10000\t50\t0\t/home\tcmd_10",
        ]);

        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(dash.result.indices.len(), 10);

        // Visible rows = 5, scrolloff = 2 -> m = min(2, (5-1)/2) = 2
        let visible_rows = 5;
        let scroll_off = 2;

        dash.selected = 0;
        dash.update_viewport(visible_rows, scroll_off);
        assert_eq!(dash.viewport_offset, 0);

        dash.selected = 3;
        dash.update_viewport(visible_rows, scroll_off);
        // selected(3) + m(2) >= viewport(0) + visible(5) -> viewport = (3 + 2 + 1) - 5 = 1
        assert_eq!(dash.viewport_offset, 1);

        dash.selected = 4;
        dash.update_viewport(visible_rows, scroll_off);
        assert_eq!(dash.viewport_offset, 2);

        // Moving back up
        dash.selected = 2;
        dash.update_viewport(visible_rows, scroll_off);
        // selected(2) < viewport(2) + m(2) -> viewport = 2 - 2 = 0
        assert_eq!(dash.viewport_offset, 0);
    }

    #[test]
    fn query_utf8_mutation() {
        let (_dir, arena) = make_test_arena(&[]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);

        dash.insert_char('h');
        dash.insert_char('i');
        dash.insert_char(' ');
        dash.insert_char('🦀');
        assert_eq!(dash.query, "hi 🦀");
        assert_eq!(dash.cursor, 4);

        dash.delete_char_before();
        assert_eq!(dash.query, "hi ");
        assert_eq!(dash.cursor, 3);

        dash.insert_char('w');
        dash.insert_char('o');
        dash.insert_char('r');
        dash.insert_char('l');
        dash.insert_char('d');
        assert_eq!(dash.query, "hi world");

        dash.delete_word_before();
        assert_eq!(dash.query, "hi ");

        dash.delete_to_start();
        assert_eq!(dash.query, "");
        assert_eq!(dash.cursor, 0);
    }

    #[test]
    fn scope_toggle_updates_search() {
        let (_dir, arena) = make_test_arena(&["1000\t50\t0\t/home\tls", "2000\t50\t0\t/tmp\tpwd"]);

        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(dash.scope, Scope::Global);
        assert_eq!(dash.result.indices.len(), 2);

        dash.toggle_scope();
        assert_eq!(dash.scope, Scope::Local);
        dash.result = search::search(&arena, &dash.query, dash.scope);
        assert_eq!(dash.result.indices.len(), 1);
        assert_eq!(arena.cmd_str(dash.result.indices[0]), "ls");
    }

    #[test]
    fn selection_movements() {
        let (_dir, arena) = make_test_arena(&[
            "1000\t50\t0\t/home\tcmd1",
            "2000\t50\t0\t/home\tcmd2",
            "3000\t50\t0\t/home\tcmd3",
        ]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(dash.selected, 0);

        dash.move_selection(1);
        assert_eq!(dash.selected, 1);

        dash.move_selection(10);
        assert_eq!(dash.selected, 2); // clamped to max index 2

        dash.move_selection(-1);
        assert_eq!(dash.selected, 1);

        dash.move_selection(-10);
        assert_eq!(dash.selected, 0); // clamped to 0
    }

    #[test]
    fn directional_panel_navigation() {
        let (_dir, arena) = make_test_arena(&["1000\t50\t0\t/home\tls"]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(dash.focus, FocusedPanel::Query);

        // Shift+Up moves Query -> Preview -> History
        dash.focus = dash.focus.up();
        assert_eq!(dash.focus, FocusedPanel::Preview);

        dash.focus = dash.focus.up();
        assert_eq!(dash.focus, FocusedPanel::History);

        // Top clamp
        dash.focus = dash.focus.up();
        assert_eq!(dash.focus, FocusedPanel::History);

        // Shift+Down moves History -> Preview -> Query
        dash.focus = dash.focus.down();
        assert_eq!(dash.focus, FocusedPanel::Preview);

        dash.focus = dash.focus.down();
        assert_eq!(dash.focus, FocusedPanel::Query);

        // Bottom clamp
        dash.focus = dash.focus.down();
        assert_eq!(dash.focus, FocusedPanel::Query);
    }

    #[test]
    fn selected_command_extraction() {
        let (_dir, arena) = make_test_arena(&["1000\t50\t0\t/home\tgit status"]);
        let dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(
            dash.selected_command(&arena),
            Some("git status".to_string())
        );

        let (_empty_dir, empty_arena) = make_test_arena(&[]);
        let mut empty_dash =
            Dashboard::new(Theme::Dark, 80, 24, Some("fallback_cmd"), &empty_arena);
        empty_dash.result = SearchResult::empty();
        assert_eq!(empty_dash.selected_command(&empty_arena), None);
    }

    #[test]
    fn preview_scrolling() {
        let (_dir, arena) = make_test_arena(&[]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);

        dash.scroll_preview(5, 20);
        assert_eq!(dash.preview_scroll, 5);

        dash.scroll_preview(50, 20);
        assert_eq!(dash.preview_scroll, 19); // max is 20 - 1 = 19

        dash.scroll_preview(-10, 20);
        assert_eq!(dash.preview_scroll, 9);

        dash.scroll_preview(-50, 20);
        assert_eq!(dash.preview_scroll, 0);
    }

    #[test]
    fn degenerate_render_no_panic() {
        let (_dir, arena) = make_test_arena(&["1000\t50\t0\t/home\tls"]);
        let dash = Dashboard::new(Theme::Dark, 10, 3, None, &arena);
        let mut token_cache = TokenCache::new();
        let mut preview_cache = PreviewCache::new();

        let layout = compute_master_layout(Rect::new(0, 0, 10, 3), 3);
        let backend = ratatui::backend::TestBackend::new(10, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| {
                render_frame(
                    f,
                    &dash,
                    &arena,
                    &mut token_cache,
                    &mut preview_cache,
                    layout,
                );
            })
            .expect("draw");
    }

    #[test]
    fn format_timestamp_validation() {
        let ts = 1_729_975_440_000_i64; // 2024-10-26
        let formatted = format_timestamp(ts);
        assert_ne!(formatted, "--:--");
        assert_eq!(formatted.len(), 12); // "DD Mmm HH:MM"
    }

    #[test]
    fn format_thousands_formatting() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(1), "1");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(50_000), "50,000");
        assert_eq!(format_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn master_layout_stability_and_zero_jitter() {
        // Master layout must produce completely deterministic, static heights
        // to prevent any layout jitter during navigation.
        let area_normal = Rect::new(0, 0, 100, 30);
        let layout_normal = compute_master_layout(area_normal, PREVIEW_PANEL_HEIGHT);

        assert_eq!(layout_normal.ceiling.height, TOP_BREATHING_ROOM);
        assert_eq!(layout_normal.actions.height, ACTION_BAR_HEIGHT);
        assert_eq!(layout_normal.preview.height, PREVIEW_PANEL_HEIGHT);
        assert_eq!(layout_normal.query.height, QUERY_BAR_HEIGHT);
        // History panel flexes to absorb the rest
        assert_eq!(
            layout_normal.history.height,
            30 - (TOP_BREATHING_ROOM + ACTION_BAR_HEIGHT + PREVIEW_PANEL_HEIGHT + QUERY_BAR_HEIGHT)
        );

        // Terminal resize to larger window preserves static preview and query heights
        let area_tall = Rect::new(0, 0, 100, 60);
        let layout_tall = compute_master_layout(area_tall, PREVIEW_PANEL_HEIGHT);
        assert_eq!(layout_tall.preview.height, PREVIEW_PANEL_HEIGHT);
        assert_eq!(layout_tall.query.height, QUERY_BAR_HEIGHT);

        // Panel widths respect horizontal margin
        assert_eq!(layout_normal.history.width, 100 - (2 * HORIZONTAL_MARGIN));
        assert_eq!(layout_normal.preview.width, 100 - (2 * HORIZONTAL_MARGIN));
        assert_eq!(layout_normal.query.width, 100 - (2 * HORIZONTAL_MARGIN));
    }

    #[test]
    fn unified_panel_block_styling_and_focus() {
        let focused_block = create_panel_block(true);
        let unfocused_block = create_panel_block(false);

        let area = Rect::new(0, 0, 80, 10);
        let inner_focused = focused_block.inner(area);
        let inner_unfocused = unfocused_block.inner(area);

        // Both blocks must share identical box model with zero padding (indicator flush against border)
        assert_eq!(inner_focused, inner_unfocused);
        assert_eq!(inner_focused.x, area.x + 1);
        assert_eq!(inner_focused.y, area.y + 1);
        assert_eq!(inner_focused.width, area.width - 2);
        assert_eq!(inner_focused.height, area.height - 2);
    }

    #[test]
    fn build_highlighted_spans_match_overlay_and_syntax_preservation() {
        let mut token_cache = TokenCache::new();
        let cmd = "git commit -m 'init'";
        let spans_syntax = syntax::tokenize(&mut token_cache, cmd);

        // 1. Without query (e.g. preview pane): pure syntax highlighting, no yellow match overlay
        let spans_no_query = build_highlighted_spans(cmd, spans_syntax, Theme::Dark, None);
        assert!(!spans_no_query.is_empty());
        for span in &spans_no_query {
            assert_ne!(
                span.style,
                Style::default()
                    .fg(Color::Indexed(215))
                    .add_modifier(Modifier::BOLD),
                "No span should have match highlight when query is None"
            );
        }

        // 2. With matching query: matched characters get Color::Yellow + BOLD overlay
        let spans_with_query =
            build_highlighted_spans(cmd, spans_syntax, Theme::Dark, Some("commit"));
        let has_yellow_bold = spans_with_query.iter().any(|span| {
            span.style.fg == Some(Color::Indexed(215))
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.content == "commit"
        });
        assert!(
            has_yellow_bold,
            "Expected 'commit' to be highlighted with Color::Yellow and BOLD"
        );

        // 3. Case-insensitive subsequence match
        let spans_case = build_highlighted_spans(cmd, spans_syntax, Theme::Dark, Some("GIT"));
        let has_git_match = spans_case.iter().any(|span| {
            span.style.fg == Some(Color::Indexed(215))
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.content == "git"
        });
        assert!(has_git_match, "Expected case-insensitive match for 'GIT'");

        // 4. Operator prefix stripping ('commit or ^commit)
        let spans_prefix = build_highlighted_spans(cmd, spans_syntax, Theme::Dark, Some("^commit"));
        let has_prefix_match = spans_prefix.iter().any(|span| {
            span.style.fg == Some(Color::Indexed(215))
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.content == "commit"
        });
        assert!(
            has_prefix_match,
            "Expected stripped query '^commit' to match 'commit'"
        );
    }

    #[test]
    fn preview_scroll_auto_reset_behavior() {
        let (_dir, arena) = make_test_arena(&[
            "1000\t50\t0\t/home\tgit status",
            "2000\t50\t0\t/home\tgit diff",
        ]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);

        // Manually scroll preview
        dash.scroll_preview(5, 50);
        assert_eq!(dash.preview_scroll, 5);

        // 1. Moving selection resets preview_scroll to 0
        dash.move_selection(1);
        assert_eq!(dash.selected, 1);
        assert_eq!(dash.preview_scroll, 0);

        // Scroll again
        dash.scroll_preview(3, 50);
        assert_eq!(dash.preview_scroll, 3);

        // 2. Toggling scope resets preview_scroll to 0
        dash.toggle_scope();
        assert_eq!(dash.preview_scroll, 0);
    }

    #[test]
    fn bottom_up_places_latest_command_at_bottom_of_panel() {
        let (_dir, arena) = make_test_arena(&[
            "1000\t50\t0\t/home\tolder_cmd",
            "2000\t50\t0\t/home\tlatest_cmd",
        ]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(dash.selected, 0);
        assert_eq!(arena.cmd_str(dash.result.indices[0]), "latest_cmd");
        assert_eq!(arena.cmd_str(dash.result.indices[1]), "older_cmd");

        let mut token_cache = TokenCache::new();
        let mut preview_cache = PreviewCache::new();
        let layout = compute_master_layout(Rect::new(0, 0, 80, 24), PREVIEW_PANEL_HEIGHT);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| {
                render_frame(
                    f,
                    &dash,
                    &arena,
                    &mut token_cache,
                    &mut preview_cache,
                    layout,
                );
            })
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let history_inner = layout.history_inner();

        // The bottom line of the history inner area
        let bottom_y = history_inner.y + history_inner.height - 1;
        let mut bottom_line_text = String::new();
        for x in history_inner.x..history_inner.x + history_inner.width {
            bottom_line_text.push(buffer[(x, bottom_y)].symbol().chars().next().unwrap_or(' '));
        }

        // Verify that latest_cmd is on the bottom line with the selection indicator `>`
        assert!(
            bottom_line_text.contains('>'),
            "Bottom line must have selection marker '>': got {bottom_line_text:?}"
        );
        assert!(
            bottom_line_text.contains("latest_cmd"),
            "Bottom line must contain the latest command: got {bottom_line_text:?}"
        );

        // Move selection UP to older command
        dash.move_selection(1);
        assert_eq!(dash.selected, 1);

        terminal
            .draw(|f| {
                render_frame(
                    f,
                    &dash,
                    &arena,
                    &mut token_cache,
                    &mut preview_cache,
                    layout,
                );
            })
            .expect("draw");

        let buffer2 = terminal.backend().buffer().clone();
        let mut row_above_bottom = String::new();
        for x in history_inner.x..history_inner.x + history_inner.width {
            row_above_bottom.push(
                buffer2[(x, bottom_y - 1)]
                    .symbol()
                    .chars()
                    .next()
                    .unwrap_or(' '),
            );
        }
        assert!(
            row_above_bottom.contains('>'),
            "Row above bottom must have selection marker '>': got {row_above_bottom:?}"
        );
        assert!(
            row_above_bottom.contains("older_cmd"),
            "Row above bottom must contain older command: got {row_above_bottom:?}"
        );
    }

    #[test]
    fn calc_preview_height_behavior() {
        // Empty or None defaults to 3 (or clamped to max_height)
        assert_eq!(calc_preview_height(None, 80, 10), 3);
        assert_eq!(calc_preview_height(Some(""), 80, 10), 3);
        assert_eq!(calc_preview_height(Some("   "), 80, 10), 3);

        // Single line command: 1 content + 2 borders = 3
        assert_eq!(calc_preview_height(Some("echo hello"), 80, 10), 3);

        // 3 line command: 3 content + 2 borders = 5
        let multi = "echo 1\necho 2\necho 3";
        assert_eq!(calc_preview_height(Some(multi), 80, 10), 5);

        // Clamped by max_height
        assert_eq!(calc_preview_height(Some(multi), 80, 4), 4);

        // Extreme small max_height
        assert_eq!(calc_preview_height(Some(multi), 80, 2), 2);
    }

    #[test]
    fn dynamic_vs_fixed_preview_height() {
        let (_dir, arena) =
            make_test_arena(&["1000\t50\t0\t/home\tline1\\nline2\\nline3\\nline4\\nline5"]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 30, None, &arena);

        // Default is Fixed: always returns PREVIEW_PANEL_HEIGHT (10)
        assert_eq!(dash.preview_mode, PreviewMode::Fixed);
        assert_eq!(dash.preview_height(&arena, 30, 80), PREVIEW_PANEL_HEIGHT);

        // Switch to Dynamic: command has 5 lines -> 5 + 2 = 7 height
        dash.preview_mode = PreviewMode::Dynamic;
        assert_eq!(dash.preview_height(&arena, 30, 80), 7);
    }

    #[test]
    fn bracketed_metadata_color_contract() {
        let border = Color::Indexed(109);
        let spans = bracketed("01 Jan 12:00", border);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content, "[");
        assert_eq!(spans[0].style.fg, Some(border));
        assert_eq!(spans[1].content, "01 Jan 12:00");
        assert_eq!(spans[1].style.fg, Some(Color::Indexed(243)));
        assert_eq!(spans[2].content, "]");
        assert_eq!(spans[2].style.fg, Some(border));
    }

    #[test]
    fn query_box_icon_padding_and_no_jump_on_input() {
        let (_dir, arena) = make_test_arena(&["1000\t50\t0\t/home\tls"]);
        let mut dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        let mut token_cache = TokenCache::new();
        let mut preview_cache = PreviewCache::new();
        let layout = compute_master_layout(Rect::new(0, 0, 80, 24), PREVIEW_PANEL_HEIGHT);
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        // 1. When query is empty: prompt icon '' is displayed at q_inner.x, cursor is to the right at q_inner.x + 2
        terminal
            .draw(|f| {
                render_frame(
                    f,
                    &dash,
                    &arena,
                    &mut token_cache,
                    &mut preview_cache,
                    layout,
                );
            })
            .expect("draw");

        let q_inner = layout.query_inner();
        let buffer_empty = terminal.backend().buffer().clone();
        assert_eq!(
            buffer_empty[(q_inner.x, q_inner.y)].symbol(),
            "",
            "When query is empty, magnifying glass icon must be rendered"
        );
        let cursor_pos = terminal.get_cursor_position().expect("cursor pos");
        assert_eq!(cursor_pos.x, q_inner.x + 2);
        assert_eq!(cursor_pos.y, q_inner.y);

        // 2. When query has input: icon disappears, text starts directly at q_inner.x (no extra spaces!)
        dash.insert_char('c');
        dash.insert_char('m');
        dash.insert_char('d');
        assert_eq!(dash.query, "cmd");
        assert_eq!(dash.cursor, 3);

        terminal
            .draw(|f| {
                render_frame(
                    f,
                    &dash,
                    &arena,
                    &mut token_cache,
                    &mut preview_cache,
                    layout,
                );
            })
            .expect("draw");

        let buffer_typed = terminal.backend().buffer().clone();
        assert_ne!(
            buffer_typed[(q_inner.x, q_inner.y)].symbol(),
            "",
            "When typing, magnifying glass icon must disappear"
        );
        assert_eq!(
            buffer_typed[(q_inner.x, q_inner.y)].symbol(),
            "c",
            "Typed text must start directly at q_inner.x, on that same spot"
        );
        assert_eq!(buffer_typed[(q_inner.x + 1, q_inner.y)].symbol(), "m");
        assert_eq!(buffer_typed[(q_inner.x + 2, q_inner.y)].symbol(), "d");

        let typed_cursor_pos = terminal.get_cursor_position().expect("cursor pos");
        assert_eq!(typed_cursor_pos.x, q_inner.x + 3);
        assert_eq!(typed_cursor_pos.y, q_inner.y);
    }
}
