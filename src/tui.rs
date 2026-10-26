//! Interactive TUI dashboard for searching and selecting shell history.
//! Uses crossterm for raw mode and ratatui for differential rendering to stderr.

use std::io::Write;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::arena::{Arena, Config, Scope, Theme};
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

/// The entire TUI state. Owned by `run_tui`, mutated in-place.
pub struct Dashboard {
    // --- input ---
    pub query:           String,
    pub cursor:          usize,

    // --- focus ---
    pub focus:           FocusedPanel,

    // --- search ---
    pub scope:           Scope,
    pub result:          SearchResult,
    pub dedup_total:     usize,

    // --- navigation ---
    pub selected:        usize,
    pub viewport_offset: usize,
    pub preview_scroll:  usize,

    // --- display ---
    pub theme:           Theme,
    pub width:           u16,
    pub height:          u16,

    // --- frame buffer ---
    pub dirty:           bool,
}

impl Dashboard {
    /// Create a new Dashboard state with initial values.
    #[must_use]
    pub fn new(
        theme: Theme,
        width: u16,
        height: u16,
        initial_q: Option<&str>,
        arena: &Arena,
    ) -> Self {
        let query = initial_q.unwrap_or("").to_owned();
        let cursor = query.chars().count();
        let scope = Scope::Global;
        let dedup_total = arena.scope_indices(scope).len();
        let result = search::search(arena, &query, scope);

        Self {
            query,
            cursor,
            focus: FocusedPanel::Query,
            scope,
            result,
            dedup_total,
            selected: 0,
            viewport_offset: 0,
            preview_scroll: 0,
            theme,
            width,
            height,
            dirty: true,
        }
    }

    /// Insert a character at current cursor position (UTF-8 safe).
    pub fn insert_char(&mut self, c: char) {
        let byte_offset = self.query
            .char_indices()
            .nth(self.cursor)
            .map_or(self.query.len(), |(i, _)| i);
        self.query.insert(byte_offset, c);
        self.cursor += 1;
    }

    /// Delete character immediately before the cursor (UTF-8 safe).
    pub fn delete_char_before(&mut self) {
        if self.cursor > 0 {
            let target_char = self.cursor - 1;
            if let Some((start_byte, ch)) = self.query.char_indices().nth(target_char) {
                let end_byte = start_byte + ch.len_utf8();
                self.query.drain(start_byte..end_byte);
                self.cursor -= 1;
            }
        }
    }

    /// Delete from start of query up to cursor (Ctrl-U).
    pub fn delete_to_start(&mut self) {
        if self.cursor > 0 {
            let byte_offset = self.query
                .char_indices()
                .nth(self.cursor)
                .map_or(self.query.len(), |(i, _)| i);
            self.query.drain(..byte_offset);
            self.cursor = 0;
        }
    }

    /// Delete previous word before cursor (Ctrl-W).
    pub fn delete_word_before(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.query.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let del_count = self.cursor - i;
        let start_byte = self.query
            .char_indices()
            .nth(i)
            .map_or(self.query.len(), |(idx, _)| idx);
        let end_byte = self.query
            .char_indices()
            .nth(self.cursor)
            .map_or(self.query.len(), |(idx, _)| idx);
        self.query.drain(start_byte..end_byte);
        self.cursor -= del_count;
    }

    /// Move cursor left one character.
    pub const fn move_cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move cursor right one character.
    pub fn move_cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.query.chars().count());
    }

    /// Move cursor to line start.
    pub const fn move_cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to line end.
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
        if delta < 0 {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            #[allow(clippy::cast_sign_loss)]
            let new_sel = self.selected.saturating_add(delta as usize);
            self.selected = new_sel.min(max_idx);
        }
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
        if visible_rows == 0 {
            self.viewport_offset = 0;
            return;
        }
        let total = self.result.indices.len();
        if total <= visible_rows {
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
        if delta < 0 {
            self.preview_scroll = self.preview_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            #[allow(clippy::cast_sign_loss)]
            let new_scroll = self.preview_scroll.saturating_add(delta as usize);
            self.preview_scroll = new_scroll.min(max_lines.saturating_sub(1));
        }
    }

    /// Resize terminal geometry.
    pub const fn resize(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
    }

    /// Get command string for current selection or fallback to query.
    #[must_use]
    pub fn selected_command(&self, arena: &Arena) -> String {
        if let Some(&idx) = self.result.indices.get(self.selected) {
            arena.cmd_str(idx).to_owned()
        } else if !self.query.is_empty() {
            self.query.clone()
        } else {
            String::new()
        }
    }
}

// ── Theme Detection ──────────────────────────────────────────────

/// Detect terminal theme using 4-tier cascade.
#[must_use]
pub fn detect_theme() -> Theme {
    if let Some(theme) = probe_env_vars() {
        return theme;
    }
    if let Some(theme) = probe_os_preferences() {
        return theme;
    }
    Theme::Dark
}

fn probe_env_vars() -> Option<Theme> {
    // Check $COLORFGBG (format: "fg;bg")
    if let Ok(val) = std::env::var("COLORFGBG")
        && let Some(bg_str) = val.rsplit(';').next()
        && let Ok(bg_num) = bg_str.trim().parse::<u8>()
    {
        return match bg_num {
            0..=6 | 8 => Some(Theme::Dark),
            7 | 9..=15 => Some(Theme::Light),
            _ => None,
        };
    }

    // Check $ITERM_PROFILE
    if let Ok(profile) = std::env::var("ITERM_PROFILE") {
        let lower = profile.to_lowercase();
        if lower.contains("light") || lower.contains("white") {
            return Some(Theme::Light);
        }
        if lower.contains("dark") || lower.contains("black") {
            return Some(Theme::Dark);
        }
    }

    None
}

fn probe_os_preferences() -> Option<Theme> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output();
        if let Ok(out) = output {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.trim() == "Dark" {
                    return Some(Theme::Dark);
                }
            } else {
                // Key not found on macOS means standard Light mode
                return Some(Theme::Light);
            }
        }
    }
    None
}

// ── Terminal Setup & Teardown ────────────────────────────────────

fn setup_terminal() -> Result<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>, Error> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stderr = std::io::stderr();
    crossterm::execute!(
        stderr,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stderr);
    let terminal = ratatui::Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>,
) {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show,
        crossterm::terminal::LeaveAlternateScreen
    );
    let _ = terminal.backend_mut().flush();
}

// ── Formatting Helpers ───────────────────────────────────────────

#[allow(clippy::cast_possible_truncation, clippy::borrow_as_ptr)]
#[must_use]
pub fn format_timestamp(timestamp_ms: i64) -> String {
    let secs = (timestamp_ms / 1000) as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();

    #[cfg(unix)]
    let success = unsafe {
        !libc::localtime_r(&secs, tm.as_mut_ptr()).is_null()
    };

    #[cfg(windows)]
    let success = unsafe {
        libc::localtime_s(tm.as_mut_ptr(), &secs) == 0
    };

    if !success {
        return "--:--".to_owned();
    }

    // SAFETY: The function succeeded and fully initialized tm.
    let tm = unsafe { tm.assume_init() };

    let month = match tm.tm_mon {
        0 => "Jan", 1 => "Feb", 2 => "Mar", 3 => "Apr",
        4 => "May", 5 => "Jun", 6 => "Jul", 7 => "Aug",
        8 => "Sep", 9 => "Oct", 10 => "Nov", 11 => "Dec",
        _ => "???",
    };

    format!("{:02} {} {:02}:{:02}", tm.tm_mday, month, tm.tm_hour, tm.tm_min)
}

fn abbreviate_path(path: &str) -> String {
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

fn format_thousands(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let commas = (len.saturating_sub(1)) / 3;
    let mut out = String::with_capacity(len + commas);

    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

// ── Unified Master Layout & Box Model ────────────────────────────

pub const TOP_BREATHING_ROOM: u16 = 1;
pub const ACTION_BAR_HEIGHT: u16 = 1;
pub const PREVIEW_PANEL_HEIGHT: u16 = 6;
pub const QUERY_BAR_HEIGHT: u16 = 3;

pub const HORIZONTAL_MARGIN: u16 = 1;
pub const PANEL_PADDING_HORIZONTAL: u16 = 1;
pub const PANEL_PADDING: Padding = Padding::horizontal(PANEL_PADDING_HORIZONTAL);

/// Master layout chunks derived from a single vertical split hierarchy.
#[derive(Debug, Clone, Copy)]
pub struct MasterLayout {
    pub ceiling: Rect,
    pub actions: Rect,
    pub history: Rect,
    pub preview: Rect,
    pub query:   Rect,
}

/// Compute master layout tree with strict static heights to guarantee zero jitter.
#[must_use]
pub fn compute_master_layout(area: Rect) -> MasterLayout {
    let preview_height = if area.height >= 14 {
        PREVIEW_PANEL_HEIGHT
    } else {
        PREVIEW_PANEL_HEIGHT.min(area.height.saturating_sub(8))
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(HORIZONTAL_MARGIN)
        .constraints([
            Constraint::Length(TOP_BREATHING_ROOM),
            Constraint::Length(ACTION_BAR_HEIGHT),
            Constraint::Min(3),                      // History List (flexible flex)
            Constraint::Length(preview_height),      // Preview Panel (strictly static)
            Constraint::Length(QUERY_BAR_HEIGHT),    // Query Bar (fixed height: 3)
        ])
        .split(area);

    MasterLayout {
        ceiling: chunks[0],
        actions: chunks[1],
        history: chunks[2],
        preview: chunks[3],
        query:   chunks[4],
    }
}

/// Focus state for styling panel borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelFocus {
    Focused,
    Unfocused,
}

/// Create a unified panel block with standard borders, rounded corners, and unified padding.
#[must_use]
pub fn create_panel_block(focus: PanelFocus) -> Block<'static> {
    let border_color = match focus {
        PanelFocus::Focused => Color::Cyan,
        PanelFocus::Unfocused => Color::DarkGray,
    };

    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .padding(PANEL_PADDING)
}

fn split_spans_into_lines(spans: Vec<Span<'_>>) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut current_line_spans = Vec::new();

    for span in spans {
        let style = span.style;
        let s = span.content.as_ref();
        for (idx, part) in s.split('\n').enumerate() {
            if idx > 0 {
                lines.push(Line::from(std::mem::take(&mut current_line_spans)));
            }
            if !part.is_empty() {
                current_line_spans.push(Span::styled(part.to_string(), style));
            }
        }
    }

    if !current_line_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(current_line_spans));
    }

    lines
}

// ── Frame Rendering ──────────────────────────────────────────────

#[allow(clippy::option_if_let_else)]
fn extract_query_term(query: &str) -> &str {
    let trimmed = query.trim();
    if let Some(rest) = trimmed.strip_prefix('^') {
        rest
    } else if let Some(rest) = trimmed.strip_prefix('\'') {
        rest
    } else if let Some(rest) = trimmed.strip_suffix('$') {
        rest
    } else {
        trimmed
    }
}

/// Zero-Destructive AST syntax highlighter with character-level query match overlay.
///
/// 1. Maps AST syntax tokens to character style slots.
/// 2. If a search query is present, performs case-insensitive subsequence match.
/// 3. Overlays matched characters with `Color::Yellow` + `BOLD` without mutating surrounding syntax.
/// 4. Coalesces adjacent characters of identical style into zero-copy `Span` slices of `cmd`.
fn build_highlighted_spans<'a>(
    cmd: &'a str,
    spans: &[syntax::Span],
    theme: Theme,
    query: Option<&str>,
) -> Vec<Span<'a>> {
    if cmd.is_empty() {
        return Vec::new();
    }

    let char_offsets: Vec<(usize, char)> = cmd.char_indices().collect();
    let char_count = char_offsets.len();
    if char_count == 0 {
        return Vec::new();
    }

    let base_style = syntax::style_for(syntax::TokenKind::Plain, theme);
    let mut char_styles = vec![base_style; char_count];

    // 1. AST syntax token layer
    for span in spans {
        let s_byte = span.start as usize;
        let e_byte = span.end as usize;
        let start_char = char_offsets
            .binary_search_by_key(&s_byte, |&(b, _)| b)
            .unwrap_or_else(|idx| idx)
            .min(char_count);
        let end_char = char_offsets
            .binary_search_by_key(&e_byte, |&(b, _)| b)
            .unwrap_or_else(|idx| idx)
            .min(char_count);

        let style = syntax::style_for(span.kind, theme);
        for s in &mut char_styles[start_char..end_char] {
            *s = style;
        }
    }

    // 2. Query match overlay (Zero-Destructive Subsequence Overlay)
    if let Some(q) = query {
        let query_term = extract_query_term(q);
        if !query_term.is_empty() {
            let query_chars: Vec<char> = query_term.chars().collect();
            let mut match_indices = std::collections::HashSet::new();
            let mut q_idx = 0;

            for (t_idx, &(_, t_ch)) in char_offsets.iter().enumerate() {
                if q_idx < query_chars.len() {
                    let q_ch = query_chars[q_idx];
                    if t_ch.eq_ignore_ascii_case(&q_ch)
                        || t_ch.to_lowercase().eq(q_ch.to_lowercase())
                    {
                        match_indices.insert(t_idx);
                        q_idx += 1;
                    }
                }
            }

            // 3. Highlight matched characters in standard ANSI Color::Yellow + BOLD
            if q_idx == query_chars.len() {
                let match_style = Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                for idx in match_indices {
                    if idx < char_styles.len() {
                        char_styles[idx] = match_style;
                    }
                }
            }
        }
    }

    // 4. Span coalescing (group adjacent characters of identical style into zero-copy slices)
    let mut out = Vec::new();
    let mut i = 0;
    while i < char_count {
        let style = char_styles[i];
        let mut j = i + 1;
        while j < char_count && char_styles[j] == style {
            j += 1;
        }

        let byte_start = char_offsets[i].0;
        let byte_end = if j < char_count {
            char_offsets[j].0
        } else {
            cmd.len()
        };

        if let Some(slice) = cmd.get(byte_start..byte_end) {
            out.push(Span::styled(slice, style));
        }
        i = j;
    }

    out
}

/// Render a single TUI frame to the ratatui Frame.
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn render_frame(
    f: &mut Frame<'_>,
    dash: &Dashboard,
    arena: &Arena,
    token_cache: &mut TokenCache,
    preview_cache: &mut PreviewCache,
) {
    let area = f.area();

    // Degenerate size handling
    if area.width < 20 || area.height < 5 {
        let msg = Paragraph::new("Terminal too small");
        f.render_widget(msg, area);
        return;
    }

    let layout = compute_master_layout(area);

    // 1. Action Buttons
    let items = [
        ("[Esc]", " Exit"),
        ("[^R]", " Scope"),
        ("[↵]", " Run"),
        ("[Tab]", " Edit"),
    ];
    let mut header_spans = Vec::new();
    for (i, (key, desc)) in items.iter().enumerate() {
        if i > 0 {
            header_spans.push(Span::raw("   "));
        }
        header_spans.push(Span::styled(*key, Style::default().fg(Color::DarkGray)));
        header_spans.push(Span::styled(
            *desc,
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ));
    }
    f.render_widget(
        Line::from(header_spans).alignment(Alignment::Center),
        layout.actions,
    );

    // 2. History List
    let history_focus = if dash.focus == FocusedPanel::History {
        PanelFocus::Focused
    } else {
        PanelFocus::Unfocused
    };
    let scope_char = match dash.scope {
        Scope::Local => "L",
        Scope::Global => "G",
    };
    let scope_spans = vec![
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled(
            scope_char,
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
    ];

    let total = dash.result.indices.len();
    let current = if total == 0 {
        0
    } else {
        dash.selected.min(total.saturating_sub(1)) + 1
    };
    let count_str = format!("{}/{}", format_thousands(current), format_thousands(total));
    let count_spans = vec![
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled(count_str, Style::default().fg(Color::DarkGray)),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
    ];

    let history_block = create_panel_block(history_focus)
        .title_top(Line::from(scope_spans).alignment(Alignment::Left))
        .title_top(Line::from(count_spans).alignment(Alignment::Right));

    let history_inner = history_block.inner(layout.history);
    let history_inner_height = history_inner.height as usize;
    let history_inner_width = history_inner.width as usize;
    let mut history_lines = Vec::with_capacity(history_inner_height);

    for row_idx in 0..history_inner_height {
        // Bottom-up alignment: row_idx = history_inner_height - 1 is the bottom-most line
        let from_bottom = (history_inner_height.saturating_sub(1)).saturating_sub(row_idx);
        let item_idx = dash.viewport_offset + from_bottom;
        if item_idx < dash.result.indices.len() {
            let arena_idx = dash.result.indices[item_idx];
            let cmd = arena.cmd_str(arena_idx);
            let is_selected = item_idx == dash.selected;

            let spans_syntax = syntax::tokenize(token_cache, cmd);
            let mut line_spans = Vec::new();

            // Cursor prefix
            if is_selected {
                line_spans.push(Span::styled(
                    "> ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ));
            } else {
                line_spans.push(Span::raw("  "));
            }

            // Command syntax spans: preserved identically without mutating style on selection
            let mut highlighted = build_highlighted_spans(cmd, spans_syntax, dash.theme, Some(&dash.query));
            line_spans.append(&mut highlighted);

            // Pad row across the entire inner width of the panel
            let used_width: usize = 2 + cmd.chars().count();
            let padding_count = history_inner_width.saturating_sub(used_width);
            if padding_count > 0 {
                line_spans.push(Span::raw(" ".repeat(padding_count)));
            }

            if is_selected {
                let sel_bg = match dash.theme {
                    Theme::Dark => Color::DarkGray,
                    Theme::Light => Color::Gray,
                };
                for span in &mut line_spans {
                    span.style = span.style.bg(sel_bg);
                }
            }

            history_lines.push(Line::from(line_spans));
        } else {
            history_lines.push(Line::raw(""));
        }
    }

    let history_widget = Paragraph::new(history_lines).block(history_block);
    f.render_widget(history_widget, layout.history);

    // 3. Preview Box
    let preview_focus = if dash.focus == FocusedPanel::Preview {
        PanelFocus::Focused
    } else {
        PanelFocus::Unfocused
    };
    let mut preview_lines = Vec::new();
    let (time_str, path_str) = if let Some(&arena_idx) = dash.result.indices.get(dash.selected) {
        let ts = arena.timestamps[arena_idx as usize];
        let cwd = arena.cwd_str(arena_idx);
        (format_timestamp(ts), abbreviate_path(cwd))
    } else {
        ("--:--".to_owned(), String::new())
    };

    let time_spans = vec![
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled(
            time_str,
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
    ];
    let path_spans = vec![
        Span::styled("[", Style::default().fg(Color::DarkGray)),
        Span::styled(
            path_str,
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ),
        Span::styled("]", Style::default().fg(Color::DarkGray)),
    ];

    let mut preview_block = create_panel_block(preview_focus)
        .title_top(Line::from(time_spans).alignment(Alignment::Left))
        .title_top(Line::from(path_spans).alignment(Alignment::Right));

    if let Some(&arena_idx) = dash.result.indices.get(dash.selected) {
        let cmd = arena.cmd_str(arena_idx);
        let preview_spans = syntax::preview(preview_cache, cmd);
        let raw_spans = build_highlighted_spans(cmd, preview_spans, dash.theme, None);
        preview_lines = split_spans_into_lines(raw_spans);
    }

    let preview_inner = preview_block.inner(layout.preview);
    let visual_rows: usize = preview_lines
        .iter()
        .map(|l| {
            let w = l.width();
            if preview_inner.width > 0 && w > preview_inner.width as usize {
                w.div_ceil(preview_inner.width as usize)
            } else {
                1
            }
        })
        .sum();

    #[allow(clippy::cast_possible_truncation)]
    let max_scroll = (visual_rows as u16).saturating_sub(preview_inner.height);
    #[allow(clippy::cast_possible_truncation)]
    let preview_scroll_u16 = (dash.preview_scroll as u16).min(max_scroll);

    if visual_rows > preview_inner.height as usize {
        preview_block = preview_block.title_bottom(
            Line::from(vec![
                Span::styled("[", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}/{} • Alt+↓/↑ scroll", preview_scroll_u16 + 1, visual_rows),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                ),
                Span::styled("]", Style::default().fg(Color::DarkGray)),
            ])
            .alignment(Alignment::Right),
        );
    }

    let preview_widget = Paragraph::new(preview_lines)
        .block(preview_block)
        .wrap(Wrap { trim: false })
        .scroll((preview_scroll_u16, 0));
    f.render_widget(preview_widget, layout.preview);

    // 4. Search Query Box (bottom)
    let query_focus = if dash.focus == FocusedPanel::Query {
        PanelFocus::Focused
    } else {
        PanelFocus::Unfocused
    };
    let query_block = create_panel_block(query_focus);
    let query_inner = query_block.inner(layout.query);

    let prompt_icon = " ";
    let query_line = Line::from(vec![
        Span::styled(prompt_icon, Style::default().fg(Color::DarkGray)),
        Span::raw(&dash.query),
    ]);
    let query_widget = Paragraph::new(query_line).block(query_block);
    f.render_widget(query_widget, layout.query);

    // Position hardware cursor strictly derived from query_inner (never manual magic offsets!)
    #[allow(clippy::cast_possible_truncation)]
    let prompt_width = prompt_icon.chars().count() as u16;
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = (query_inner.x + prompt_width + dash.cursor as u16)
        .min(query_inner.x + query_inner.width.saturating_sub(1));
    let cursor_y = query_inner.y;
    f.set_cursor_position((cursor_x, cursor_y));
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

    let mut token_cache = TokenCache::new();
    let mut preview_cache = PreviewCache::new();

    let mut terminal = setup_terminal()?;

    let outcome = loop {
        if dash.dirty {
            let layout = compute_master_layout(Rect::new(0, 0, dash.width, dash.height));
            let history_inner = create_panel_block(PanelFocus::Unfocused).inner(layout.history);
            let visible_rows = history_inner.height as usize;
            dash.update_viewport(visible_rows, config.scroll_off);

            terminal.draw(|f| {
                render_frame(f, &dash, arena, &mut token_cache, &mut preview_cache);
            })?;
            dash.dirty = false;
        }

        match crossterm::event::read()? {
            // --- Query Mutation ---
            Event::Key(KeyEvent { code: KeyCode::Char(c), modifiers, .. }) => {
                if modifiers == KeyModifiers::CONTROL {
                    match c {
                        'r' | 'g' => {
                            dash.focus = FocusedPanel::Query;
                            dash.toggle_scope();
                            dash.dedup_total = arena.scope_indices(dash.scope).len();
                            dash.result = search::search(arena, &dash.query, dash.scope);
                            dash.selected = 0;
                            dash.preview_scroll = 0;
                            dash.dirty = true;
                        }
                        'u' => {
                            dash.focus = FocusedPanel::Query;
                            dash.delete_to_start();
                            dash.result = search::search(arena, &dash.query, dash.scope);
                            dash.selected = 0;
                            dash.preview_scroll = 0;
                            dash.dirty = true;
                        }
                        'w' => {
                            dash.focus = FocusedPanel::Query;
                            dash.delete_word_before();
                            dash.result = search::search(arena, &dash.query, dash.scope);
                            dash.selected = 0;
                            dash.preview_scroll = 0;
                            dash.dirty = true;
                        }
                        'a' => {
                            dash.focus = FocusedPanel::Query;
                            dash.move_cursor_home();
                            dash.dirty = true;
                        }
                        'e' => {
                            dash.focus = FocusedPanel::Query;
                            dash.move_cursor_end();
                            dash.dirty = true;
                        }
                        'p' => {
                            dash.focus = FocusedPanel::History;
                            dash.move_selection(1);
                            dash.dirty = true;
                        }
                        'n' => {
                            dash.focus = FocusedPanel::History;
                            dash.move_selection(-1);
                            dash.dirty = true;
                        }
                        'c' | 'd' => {
                            break Outcome::Cancel;
                        }
                        _ => {}
                    }
                } else if modifiers == KeyModifiers::ALT {
                    match c {
                        'j' => {
                            dash.focus = FocusedPanel::Preview;
                            dash.scroll_preview(1, 1000);
                            dash.dirty = true;
                        }
                        'k' => {
                            dash.focus = FocusedPanel::Preview;
                            dash.scroll_preview(-1, 1000);
                            dash.dirty = true;
                        }
                        _ => {}
                    }
                } else {
                    dash.focus = FocusedPanel::Query;
                    dash.insert_char(c);
                    dash.result = search::search(arena, &dash.query, dash.scope);
                    dash.selected = 0;
                    dash.preview_scroll = 0;
                    dash.dirty = true;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                dash.focus = FocusedPanel::Query;
                dash.delete_char_before();
                dash.result = search::search(arena, &dash.query, dash.scope);
                dash.selected = 0;
                dash.preview_scroll = 0;
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Left, .. }) => {
                dash.focus = FocusedPanel::Query;
                dash.move_cursor_left();
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Right, .. }) => {
                dash.focus = FocusedPanel::Query;
                dash.move_cursor_right();
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Home, .. }) => {
                dash.focus = FocusedPanel::Query;
                dash.move_cursor_home();
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::End, .. }) => {
                dash.focus = FocusedPanel::Query;
                dash.move_cursor_end();
                dash.dirty = true;
            }

            // --- Navigation ---
            Event::Key(KeyEvent { code: KeyCode::Up, modifiers, .. }) => {
                if modifiers == KeyModifiers::ALT {
                    dash.focus = FocusedPanel::Preview;
                    dash.scroll_preview(-1, 1000);
                } else {
                    dash.focus = FocusedPanel::History;
                    dash.move_selection(1);
                }
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Down, modifiers, .. }) => {
                if modifiers == KeyModifiers::ALT {
                    dash.focus = FocusedPanel::Preview;
                    dash.scroll_preview(1, 1000);
                } else {
                    dash.focus = FocusedPanel::History;
                    dash.move_selection(-1);
                }
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::PageUp, .. }) => {
                dash.focus = FocusedPanel::History;
                dash.move_selection(10);
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::PageDown, .. }) => {
                dash.focus = FocusedPanel::History;
                dash.move_selection(-10);
                dash.dirty = true;
            }

            // --- Mouse scroll & click support ---
            Event::Mouse(mouse) => {
                let layout = compute_master_layout(Rect::new(0, 0, dash.width, dash.height));
                let is_over_preview = mouse.column >= layout.preview.x
                    && mouse.column < layout.preview.x + layout.preview.width
                    && mouse.row >= layout.preview.y
                    && mouse.row < layout.preview.y + layout.preview.height;

                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        if is_over_preview {
                            dash.focus = FocusedPanel::Preview;
                            dash.scroll_preview(-1, 1000);
                        } else {
                            dash.focus = FocusedPanel::History;
                            dash.move_selection(1);
                        }
                        dash.dirty = true;
                    }
                    MouseEventKind::ScrollDown => {
                        if is_over_preview {
                            dash.focus = FocusedPanel::Preview;
                            dash.scroll_preview(1, 1000);
                        } else {
                            dash.focus = FocusedPanel::History;
                            dash.move_selection(-1);
                        }
                        dash.dirty = true;
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        let history_block = create_panel_block(PanelFocus::Unfocused);
                        let history_inner = history_block.inner(layout.history);
                        if mouse.column >= history_inner.x
                            && mouse.column < history_inner.x + history_inner.width
                            && mouse.row >= history_inner.y
                            && mouse.row < history_inner.y + history_inner.height
                        {
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
                        } else {
                            let query_block = create_panel_block(PanelFocus::Unfocused);
                            let query_inner = query_block.inner(layout.query);
                            if mouse.column >= query_inner.x
                                && mouse.column < query_inner.x + query_inner.width
                                && mouse.row >= query_inner.y
                                && mouse.row < query_inner.y + query_inner.height
                            {
                                dash.focus = FocusedPanel::Query;
                                dash.dirty = true;
                            }
                        }
                    }
                    _ => {}
                }
            }

            // --- Terminal Actions ---
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                let cmd = dash.selected_command(arena);
                break Outcome::Execute(cmd);
            }
            Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => {
                let cmd = dash.selected_command(arena);
                break Outcome::Edit(cmd);
            }
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                break Outcome::Cancel;
            }

            // --- Window Resize ---
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
        let (_dir, arena) = make_test_arena(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/tmp\tpwd",
        ]);

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
    fn selected_command_extraction() {
        let (_dir, arena) = make_test_arena(&["1000\t50\t0\t/home\tgit status"]);
        let dash = Dashboard::new(Theme::Dark, 80, 24, None, &arena);
        assert_eq!(dash.selected_command(&arena), "git status");

        let (_empty_dir, empty_arena) = make_test_arena(&[]);
        let mut empty_dash = Dashboard::new(Theme::Dark, 80, 24, Some("fallback_cmd"), &empty_arena);
        empty_dash.result = SearchResult::empty();
        assert_eq!(empty_dash.selected_command(&empty_arena), "fallback_cmd");
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

        let backend = ratatui::backend::TestBackend::new(10, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal.draw(|f| {
            render_frame(f, &dash, &arena, &mut token_cache, &mut preview_cache);
        }).expect("draw");
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
        let layout_normal = compute_master_layout(area_normal);

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
        let layout_tall = compute_master_layout(area_tall);
        assert_eq!(layout_tall.preview.height, PREVIEW_PANEL_HEIGHT);
        assert_eq!(layout_tall.query.height, QUERY_BAR_HEIGHT);

        // Panel widths respect horizontal margin
        assert_eq!(layout_normal.history.width, 100 - (2 * HORIZONTAL_MARGIN));
        assert_eq!(layout_normal.preview.width, 100 - (2 * HORIZONTAL_MARGIN));
        assert_eq!(layout_normal.query.width, 100 - (2 * HORIZONTAL_MARGIN));
    }

    #[test]
    fn unified_panel_block_styling_and_focus() {
        let focused_block = create_panel_block(PanelFocus::Focused);
        let unfocused_block = create_panel_block(PanelFocus::Unfocused);

        let area = Rect::new(0, 0, 80, 10);
        let inner_focused = focused_block.inner(area);
        let inner_unfocused = unfocused_block.inner(area);

        // Both blocks must share identical box model and inner padding
        assert_eq!(inner_focused, inner_unfocused);
        assert_eq!(inner_focused.x, area.x + 1 + PANEL_PADDING_HORIZONTAL);
        assert_eq!(inner_focused.y, area.y + 1);
        assert_eq!(
            inner_focused.width,
            area.width - 2 - (2 * PANEL_PADDING_HORIZONTAL)
        );
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
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                "No span should have match highlight when query is None"
            );
        }

        // 2. With matching query: matched characters get Color::Yellow + BOLD overlay
        let spans_with_query = build_highlighted_spans(cmd, spans_syntax, Theme::Dark, Some("commit"));
        let has_yellow_bold = spans_with_query.iter().any(|span| {
            span.style.fg == Some(Color::Yellow)
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.content == "commit"
        });
        assert!(has_yellow_bold, "Expected 'commit' to be highlighted with Color::Yellow and BOLD");

        // 3. Case-insensitive subsequence match
        let spans_case = build_highlighted_spans(cmd, spans_syntax, Theme::Dark, Some("GIT"));
        let has_git_match = spans_case.iter().any(|span| {
            span.style.fg == Some(Color::Yellow)
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.content == "git"
        });
        assert!(has_git_match, "Expected case-insensitive match for 'GIT'");

        // 4. Operator prefix stripping ('commit or ^commit)
        let spans_prefix = build_highlighted_spans(cmd, spans_syntax, Theme::Dark, Some("^commit"));
        let has_prefix_match = spans_prefix.iter().any(|span| {
            span.style.fg == Some(Color::Yellow)
                && span.style.add_modifier.contains(Modifier::BOLD)
                && span.content == "commit"
        });
        assert!(has_prefix_match, "Expected stripped query '^commit' to match 'commit'");
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
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| {
                render_frame(f, &dash, &arena, &mut token_cache, &mut preview_cache);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let layout = compute_master_layout(Rect::new(0, 0, 80, 24));
        let history_block = create_panel_block(PanelFocus::Unfocused);
        let history_inner = history_block.inner(layout.history);

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
                render_frame(f, &dash, &arena, &mut token_cache, &mut preview_cache);
            })
            .expect("draw");

        let buffer2 = terminal.backend().buffer().clone();
        let mut row_above_bottom = String::new();
        for x in history_inner.x..history_inner.x + history_inner.width {
            row_above_bottom.push(buffer2[(x, bottom_y - 1)].symbol().chars().next().unwrap_or(' '));
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
}
