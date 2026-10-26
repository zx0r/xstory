//! Interactive TUI dashboard for searching and selecting shell history.
//! Uses crossterm for raw mode and ratatui for differential rendering to stderr.

use std::io::Write;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
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

/// The entire TUI state. Owned by `run_tui`, mutated in-place.
pub struct Dashboard {
    // --- input ---
    pub query:           String,
    pub cursor:          usize,

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

fn calc_preview_height(cmd: &str, width: u16, max_height: u16) -> u16 {
    if max_height <= 2 {
        return max_height;
    }
    if cmd.trim().is_empty() {
        return 3.min(max_height);
    }
    let inner_width = width.saturating_sub(6).max(1);
    let mut total_lines: u16 = 0;
    for line in cmd.lines() {
        #[allow(clippy::cast_possible_truncation)]
        let char_count = line.chars().count() as u16;
        let wrapped = char_count.max(1).div_ceil(inner_width);
        total_lines = total_lines.saturating_add(wrapped);
    }
    let max_content = max_height.saturating_sub(2);
    total_lines.clamp(1, max_content) + 2
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

fn build_highlighted_spans<'a>(
    cmd: &'a str,
    spans: &[syntax::Span],
    theme: Theme,
) -> Vec<Span<'a>> {
    let bytes = cmd.as_bytes();
    let mut out = Vec::new();
    let mut last_end = 0_usize;

    for span in spans {
        let s = span.start as usize;
        let e = span.end as usize;
        if s > last_end && s <= bytes.len()
            && let Ok(gap) = std::str::from_utf8(&bytes[last_end..s])
        {
            out.push(Span::raw(gap));
        }
        if e <= bytes.len() && s < e
            && let Ok(tok) = std::str::from_utf8(&bytes[s..e])
        {
            let style = syntax::style_for(span.kind, theme);
            out.push(Span::styled(tok, style));
        }
        last_end = e;
    }

    if last_end < bytes.len()
        && let Ok(tail) = std::str::from_utf8(&bytes[last_end..])
    {
        out.push(Span::raw(tail));
    }

    if out.is_empty() && !cmd.is_empty() {
        out.push(Span::raw(cmd));
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

    let max_preview_h = (area.height / 2).clamp(6, 20).min(area.height.saturating_sub(9));
    let preview_height = if let Some(&arena_idx) = dash.result.indices.get(dash.selected) {
        let cmd = arena.cmd_str(arena_idx);
        calc_preview_height(cmd, area.width, max_preview_h)
    } else {
        3.min(max_preview_h)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(1)
        .constraints([
            Constraint::Length(1),                  // 0: Top breathing room from ceiling
            Constraint::Length(1),                  // 1: Action buttons
            Constraint::Min(3),                     // 2: History list [G] [count]
            Constraint::Length(preview_height),     // 3: Preview box [Date] [path]
            Constraint::Length(3),                  // 4: Search Query box (bottom)
        ])
        .split(area);

    // 1. Action Buttons
    let items = [
        ("[Esc]", " Exit"),
        ("[^G]", " Scope"),
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
        chunks[1],
    );

    // 2. History List
    let history_inner_height = chunks[2].height.saturating_sub(2) as usize;
    let history_inner_width = chunks[2].width.saturating_sub(2) as usize;
    let mut history_lines = Vec::with_capacity(history_inner_height);

    for row_idx in 0..history_inner_height {
        let item_idx = dash.viewport_offset + row_idx;
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

            // Command syntax spans: preserved across all rows without flipping state on selection
            let mut highlighted = build_highlighted_spans(cmd, spans_syntax, dash.theme);
            if is_selected {
                for s in &mut highlighted {
                    s.style = s.style.add_modifier(Modifier::BOLD | Modifier::ITALIC);
                }
            }
            line_spans.append(&mut highlighted);

            // Pad row across the entire width of the panel
            let used_width: usize = 2 + cmd.chars().count();
            let padding_count = history_inner_width.saturating_sub(used_width);
            if padding_count > 0 {
                line_spans.push(Span::raw(" ".repeat(padding_count)));
            }

            if is_selected {
                let sel_bg = match dash.theme {
                    Theme::Dark => Color::Rgb(34, 38, 56),
                    Theme::Light => Color::Rgb(232, 236, 242),
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

    let history_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title_top(Line::from(scope_spans).alignment(Alignment::Left))
        .title_top(Line::from(count_spans).alignment(Alignment::Right));
    let history_widget = Paragraph::new(history_lines).block(history_block);
    f.render_widget(history_widget, chunks[2]);

    // 3. Preview Box
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

    let preview_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(time_spans).alignment(Alignment::Left))
        .title_top(Line::from(path_spans).alignment(Alignment::Right));

    if let Some(&arena_idx) = dash.result.indices.get(dash.selected) {
        let cmd = arena.cmd_str(arena_idx);
        let preview_spans = syntax::preview(preview_cache, cmd);
        let raw_spans = build_highlighted_spans(cmd, preview_spans, dash.theme);
        preview_lines = split_spans_into_lines(raw_spans);
    }

    #[allow(clippy::cast_possible_truncation)]
    let preview_scroll_u16 = dash.preview_scroll as u16;
    let preview_widget = Paragraph::new(preview_lines)
        .block(preview_block)
        .wrap(Wrap { trim: false })
        .scroll((preview_scroll_u16, 0));
    f.render_widget(preview_widget, chunks[3]);

    // 4. Search Query Box (bottom)
    let query_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray));

    let query_line = Line::from(vec![
        Span::styled(" ", Style::default().fg(Color::DarkGray)),
        Span::raw(&dash.query),
    ]);
    let query_widget = Paragraph::new(query_line).block(query_block);
    f.render_widget(query_widget, chunks[4]);

    // Position hardware cursor
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = (chunks[4].x + 3 + dash.cursor as u16).min(chunks[4].x + chunks[4].width.saturating_sub(2));
    let cursor_y = chunks[4].y + 1;
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
            let max_preview_h = (dash.height / 2).clamp(6, 20).min(dash.height.saturating_sub(9));
            let preview_h = if let Some(&arena_idx) = dash.result.indices.get(dash.selected) {
                let cmd = arena.cmd_str(arena_idx);
                calc_preview_height(cmd, dash.width, max_preview_h)
            } else {
                3.min(max_preview_h)
            };
            let history_total_h = dash.height.saturating_sub(5 + preview_h);
            let visible_rows = history_total_h.saturating_sub(2) as usize;
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
                        'g' => {
                            dash.toggle_scope();
                            dash.dedup_total = arena.scope_indices(dash.scope).len();
                            dash.result = search::search(arena, &dash.query, dash.scope);
                            dash.selected = 0;
                            dash.dirty = true;
                        }
                        'u' => {
                            dash.delete_to_start();
                            dash.result = search::search(arena, &dash.query, dash.scope);
                            dash.selected = 0;
                            dash.dirty = true;
                        }
                        'w' => {
                            dash.delete_word_before();
                            dash.result = search::search(arena, &dash.query, dash.scope);
                            dash.selected = 0;
                            dash.dirty = true;
                        }
                        'a' => {
                            dash.move_cursor_home();
                            dash.dirty = true;
                        }
                        'e' => {
                            dash.move_cursor_end();
                            dash.dirty = true;
                        }
                        'p' => {
                            dash.move_selection(-1);
                            dash.dirty = true;
                        }
                        'n' => {
                            dash.move_selection(1);
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
                            dash.scroll_preview(1, 1000);
                            dash.dirty = true;
                        }
                        'k' => {
                            dash.scroll_preview(-1, 1000);
                            dash.dirty = true;
                        }
                        _ => {}
                    }
                } else {
                    dash.insert_char(c);
                    dash.result = search::search(arena, &dash.query, dash.scope);
                    dash.selected = 0;
                    dash.dirty = true;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                dash.delete_char_before();
                dash.result = search::search(arena, &dash.query, dash.scope);
                dash.selected = 0;
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Left, .. }) => {
                dash.move_cursor_left();
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Right, .. }) => {
                dash.move_cursor_right();
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Home, .. }) => {
                dash.move_cursor_home();
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::End, .. }) => {
                dash.move_cursor_end();
                dash.dirty = true;
            }

            // --- Navigation ---
            Event::Key(KeyEvent { code: KeyCode::Up, modifiers, .. }) => {
                if modifiers == KeyModifiers::ALT {
                    dash.scroll_preview(-1, 1000);
                } else {
                    dash.move_selection(-1);
                }
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Down, modifiers, .. }) => {
                if modifiers == KeyModifiers::ALT {
                    dash.scroll_preview(1, 1000);
                } else {
                    dash.move_selection(1);
                }
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::PageUp, .. }) => {
                dash.move_selection(-10);
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::PageDown, .. }) => {
                dash.move_selection(10);
                dash.dirty = true;
            }

            // --- Mouse scroll support ---
            Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollUp, .. }) => {
                dash.move_selection(-1);
                dash.dirty = true;
            }
            Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, .. }) => {
                dash.move_selection(1);
                dash.dirty = true;
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
}
