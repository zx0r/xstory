//! Interactive TUI dashboard for searching and selecting shell history.
//! Uses crossterm for raw mode and ratatui for differential rendering to stderr.

use std::io::Write;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
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

fn format_relative_time(timestamp_ms: i64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let diff_ms = now_ms.saturating_sub(timestamp_ms);
    let secs = diff_ms / 1000;
    if secs < 60 {
        "<1m".to_owned()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn format_duration(duration_ms: u32) -> String {
    if duration_ms < 1000 {
        format!("{duration_ms}ms")
    } else if duration_ms < 60_000 {
        format!("{:.1}s", f64::from(duration_ms) / 1000.0)
    } else {
        format!("{}m{}s", duration_ms / 60_000, (duration_ms % 60_000) / 1000)
    }
}

fn format_exit_status(exit: i32) -> (String, Style) {
    if exit == 0 {
        ("0 (ok)".to_owned(), Style::default().fg(Color::DarkGray))
    } else {
        let label = match exit {
            1 => "failed",
            2 => "misuse / syntax error",
            126 => "permission denied",
            127 => "command not found",
            130 => "interrupted (SIGINT)",
            137 => "killed (SIGKILL)",
            143 => "terminated (SIGTERM)",
            _ if exit < 0 => "abnormal termination",
            _ if (128..=192).contains(&exit) => "interrupted by signal",
            _ => "failed",
        };
        (
            format!("{exit} ({label})"),
            Style::default().fg(Color::Yellow),
        )
    }
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),       // Search input
            Constraint::Percentage(55),  // History list
            Constraint::Percentage(45),  // Preview
            Constraint::Length(1),       // Footer
        ])
        .split(area);

    // 1. Search Query Box
    let scope_label = match dash.scope {
        Scope::Global => "Global",
        Scope::Local => "Local",
    };
    let total_matched = dash.result.indices.len();
    let query_title = format!(" Search [{scope_label}: {total_matched}/{}] ", dash.dedup_total);
    let query_block = Block::default()
        .borders(Borders::ALL)
        .title(query_title)
        .border_style(Style::default().fg(Color::Cyan));

    let query_text = format!("> {}", dash.query);
    let query_widget = Paragraph::new(query_text).block(query_block);
    f.render_widget(query_widget, chunks[0]);

    // Position hardware cursor
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = (chunks[0].x + 3 + dash.cursor as u16).min(chunks[0].x + chunks[0].width.saturating_sub(2));
    let cursor_y = chunks[0].y + 1;
    f.set_cursor_position((cursor_x, cursor_y));

    // 2. History List
    let history_inner_height = chunks[1].height.saturating_sub(2) as usize;
    let mut history_lines = Vec::with_capacity(history_inner_height);

    for row_idx in 0..history_inner_height {
        let item_idx = dash.viewport_offset + row_idx;
        if item_idx < dash.result.indices.len() {
            let arena_idx = dash.result.indices[item_idx];
            let cmd = arena.cmd_str(arena_idx);
            let exit_code = arena.exit_codes[arena_idx as usize];
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

            // Command syntax spans: failed/interrupted commands are dimmed to DarkGray
            let mut highlighted = build_highlighted_spans(cmd, spans_syntax, dash.theme);
            if exit_code != 0 {
                for s in &mut highlighted {
                    s.style = Style::default().fg(Color::DarkGray);
                }
            }
            if is_selected {
                for s in &mut highlighted {
                    s.style = s.style.add_modifier(Modifier::BOLD);
                }
            }
            line_spans.append(&mut highlighted);

            history_lines.push(Line::from(line_spans));
        } else {
            history_lines.push(Line::raw(""));
        }
    }

    let history_block = Block::default()
        .borders(Borders::ALL)
        .title(" History ")
        .border_style(Style::default().fg(Color::DarkGray));
    let history_widget = Paragraph::new(history_lines).block(history_block);
    f.render_widget(history_widget, chunks[1]);

    // 3. Preview Box
    let mut preview_lines = Vec::new();
    if let Some(&arena_idx) = dash.result.indices.get(dash.selected) {
        let cmd = arena.cmd_str(arena_idx);
        let cwd = arena.cwd_str(arena_idx);
        let ts = arena.timestamps[arena_idx as usize];
        let dur = arena.durations[arena_idx as usize];
        let exit = arena.exit_codes[arena_idx as usize];

        let rel_time = format_relative_time(ts);
        let dur_str = format_duration(dur);

        // Header line 1: directory
        preview_lines.push(Line::from(vec![
            Span::styled("  Dir: ", Style::default().fg(Color::DarkGray)),
            Span::styled(cwd, Style::default().fg(Color::Yellow)),
        ]));

        // Header line 2: stats
        let (exit_str, exit_style) = format_exit_status(exit);
        preview_lines.push(Line::from(vec![
            Span::styled("  Time: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{rel_time} ago  ")),
            Span::styled("Duration: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{dur_str}  ")),
            Span::styled("Exit: ", Style::default().fg(Color::DarkGray)),
            Span::styled(exit_str, exit_style),
        ]));

        // Separator
        preview_lines.push(Line::styled(
            "  ────────────────────────────────────────",
            Style::default().fg(Color::DarkGray),
        ));

        // Command lines with preview syntax highlighting
        let preview_spans = syntax::preview(preview_cache, cmd);
        for cmd_line in cmd.lines() {
            let mut l_spans = vec![Span::raw("  ")];
            l_spans.append(&mut build_highlighted_spans(cmd_line, preview_spans, dash.theme));
            preview_lines.push(Line::from(l_spans));
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    let preview_scroll_u16 = dash.preview_scroll as u16;
    let preview_block = Block::default()
        .borders(Borders::ALL)
        .title(" Preview ")
        .border_style(Style::default().fg(Color::DarkGray));
    let preview_widget = Paragraph::new(preview_lines)
        .block(preview_block)
        .scroll((preview_scroll_u16, 0));
    f.render_widget(preview_widget, chunks[2]);

    // 4. Footer
    let footer_text = Line::from(vec![
        Span::styled(" [^G] ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("Scope  "),
        Span::styled("[Enter] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw("Run  "),
        Span::styled("[Tab] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("Edit  "),
        Span::styled("[Alt+J/K] ", Style::default().fg(Color::Magenta)),
        Span::raw("Preview Scroll  "),
        Span::styled("[Esc] ", Style::default().fg(Color::Red)),
        Span::raw("Cancel"),
    ]);
    f.render_widget(Paragraph::new(footer_text), chunks[3]);
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
            let history_inner_height = dash.height.saturating_sub(4) as usize * 55 / 100;
            let visible_rows = history_inner_height.saturating_sub(2);
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
    fn exit_status_formatting() {
        let (zero_str, zero_style) = format_exit_status(0);
        assert_eq!(zero_str, "0 (ok)");
        assert_eq!(zero_style.fg, Some(Color::DarkGray));

        let (fail_str, fail_style) = format_exit_status(1);
        assert_eq!(fail_str, "1 (failed)");
        assert_eq!(fail_style.fg, Some(Color::Yellow));

        let (sigint_str, _) = format_exit_status(130);
        assert_eq!(sigint_str, "130 (interrupted (SIGINT))");

        let (neg_str, _) = format_exit_status(-1);
        assert_eq!(neg_str, "-1 (abnormal termination)");
    }
}
