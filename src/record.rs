//! Ingestion pipeline: filter → serialize → atomic append.
//! Hot path: ≤1 allocation. Borrows all inputs.

use crate::arena::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::arena::{Config, fnv1a};

// ── Verdict ──────────────────────────────────────────────────────

/// Policy verdict for a command string.
enum Verdict {
    Allow,
    Deny,
    Mask,
}

// ── Escape ───────────────────────────────────────────────────────

/// Escape `\t`, `\n`, `\\` in a string for TSV serialization.
/// Returns true if any escaping was needed.
fn escape_into(s: &str, buf: &mut Vec<u8>) -> bool {
    let mut escaped = false;
    for &b in s.as_bytes() {
        match b {
            b'\t' => {
                buf.extend_from_slice(b"\\t");
                escaped = true;
            }
            b'\n' => {
                buf.extend_from_slice(b"\\n");
                escaped = true;
            }
            b'\\' => {
                buf.extend_from_slice(b"\\\\");
                escaped = true;
            }
            _ => buf.push(b),
        }
    }
    escaped
}

// ── Policy ───────────────────────────────────────────────────────

/// Check if command should be recorded, denied, or masked.
fn evaluate(cmd: &str, config: &Config) -> Verdict {
    if cmd.trim().is_empty() || cmd.starts_with(' ') {
        return Verdict::Deny;
    }
    let base = cmd.split_whitespace().next().unwrap_or("");
    if config.exclude_commands.iter().any(|excl| excl == base) {
        return Verdict::Deny;
    }
    if config
        .secret_patterns
        .iter()
        .any(|pat| cmd.contains(pat.as_str()))
    {
        return Verdict::Mask;
    }
    Verdict::Allow
}

/// Mask a command that contains secret patterns.
/// Replaces sensitive flags and keys along with their values with `***`.
fn mask_command(cmd: &str, config: &Config) -> String {
    let mut masked = cmd.to_owned();
    for pat in &config.secret_patterns {
        while let Some(pos) = masked.find(pat.as_str()) {
            let after_pat = pos + pat.len();
            let rest = &masked[after_pat..];
            let end = if rest.starts_with('=') || rest.starts_with(':') {
                let val_rest = &rest[1..];
                let val_end = val_rest
                    .find(|c: char| c.is_whitespace())
                    .unwrap_or(val_rest.len());
                1 + val_end
            } else if rest.starts_with(' ') {
                let trimmed = rest.trim_start();
                let spaces = rest.len() - trimmed.len();
                let val_end = trimmed
                    .find(|c: char| c.is_whitespace())
                    .unwrap_or(trimmed.len());
                spaces + val_end
            } else {
                rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len())
            };
            masked.replace_range(pos..after_pat + end, "***");
        }
    }
    masked
}

// ── Public API ───────────────────────────────────────────────────

/// Record a command to the history file. Hot path: filter → serialize → atomic append.
///
/// `cmd_parts`: command tokens (joined with space)
/// `exit_code`: process exit code
/// `duration_ms`: execution duration in milliseconds
/// `cwd`: working directory (falls back to `$PWD` or "?")
///
/// # Errors
/// Returns `Error::Io` if writing to the history file fails.
#[allow(clippy::cast_possible_wrap)]
pub fn record(
    cmd_parts: &[String],
    exit_code: i32,
    duration_ms: u64,
    cwd: Option<&str>,
    config: &Config,
) -> Result<(), crate::error::Error> {
    let cwd_env = if cwd.is_none() {
        std::env::var("PWD").ok()
    } else {
        None
    };
    let cwd_ref = cwd.or(cwd_env.as_deref()).unwrap_or("?");
    record_to_path(
        cmd_parts,
        exit_code,
        duration_ms,
        Some(cwd_ref),
        config,
        &config.history_path,
    )
}

/// Record with explicit path override (for testing).
///
/// # Errors
/// Returns `Error::Io` if writing to the specified path fails.
#[allow(dead_code, clippy::cast_possible_wrap)]
pub fn record_to_path(
    cmd_parts: &[String],
    exit_code: i32,
    duration_ms: u64,
    cwd: Option<&str>,
    config: &Config,
    path: &Path,
) -> Result<(), crate::error::Error> {
    let cmd = cmd_parts.join(" ");

    let final_cmd = match evaluate(&cmd, config) {
        Verdict::Deny => return Ok(()),
        Verdict::Allow => cmd,
        Verdict::Mask => mask_command(&cmd, config),
    };

    let cwd_ref = cwd.unwrap_or("?");
    let timestamp_ms = now_epoch_ms();

    let mut buf = Vec::with_capacity(64 + final_cmd.len() + cwd_ref.len());

    let _ = write!(buf, "{timestamp_ms}\t{duration_ms}\t{exit_code}\t");
    escape_into(cwd_ref, &mut buf);
    buf.push(b'\t');
    escape_into(&final_cmd, &mut buf);
    buf.push(b'\n');

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;

    file.write_all(&buf)?;
    Ok(())
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// ── History Import / Export ──────────────────────────────────────

#[must_use]
pub fn detect_shell_history() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HISTFILE")
        .map(PathBuf::from)
        .filter(|p| p.exists())
    {
        return Some(p);
    }
    let h = PathBuf::from(std::env::var_os("HOME")?);
    let sh = std::env::var("SHELL").unwrap_or_default();
    let preferred = if sh.ends_with("fish") {
        ".local/share/fish/fish_history"
    } else if sh.ends_with("bash") {
        ".bash_history"
    } else {
        ".zsh_history"
    };
    [
        preferred,
        ".zsh_history",
        ".local/share/fish/fish_history",
        ".bash_history",
    ]
    .into_iter()
    .map(|rel| h.join(rel))
    .find(|p| p.exists())
}

/// In-place Zsh 0x83 unmetafy: zero allocations, instant SIMD skip on clean ASCII/UTF-8.
fn unmetafy_in_place(buf: &mut Vec<u8>) {
    if !buf.contains(&0x83) {
        return;
    }
    let (mut r, mut w) = (0, 0);
    while r < buf.len() {
        if buf[r] == 0x83 && r + 1 < buf.len() {
            buf[w] = buf[r + 1] ^ 0x20;
            r += 2;
        } else {
            buf[w] = buf[r];
            r += 1;
        }
        w += 1;
    }
    buf.truncate(w);
}

/// Unified zero-alloc unescaper delegating to canonical `arena::unescape_into`.
#[inline]
fn unescape_into(s: &[u8], out: &mut Vec<u8>) {
    out.clear();
    crate::arena::unescape_into(s, out);
}

#[inline]
fn has_odd_trailing_backslashes(b: &[u8]) -> bool {
    b.iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 1
}

enum InStream {
    File(BufReader<std::fs::File>),
    Stdin(BufReader<std::io::Stdin>),
}

fn read_stream_line(stream: &mut InStream, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    match stream {
        InStream::File(r) => r.read_until(b'\n', buf),
        InStream::Stdin(r) => r.read_until(b'\n', buf),
    }
}

/// Export xstory history to a file path (0600) or stdout, respecting Scope.
///
/// # Errors
/// Returns `Error::Io` on read or write failure.
pub fn export_history(
    out_path: Option<&str>,
    scope: crate::arena::Scope,
    config: &Config,
) -> Result<(), crate::error::Error> {
    export_history_from_path(out_path, scope, &config.history_path)
}

/// Export history from a specific path (for testing).
///
/// # Errors
/// Returns `Error::Io` on read or write failure.
pub fn export_history_from_path(
    out_path: Option<&str>,
    scope: crate::arena::Scope,
    history_path: &Path,
) -> Result<(), crate::error::Error> {
    if !history_path.exists() {
        return Ok(());
    }
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let mut reader = BufReader::with_capacity(64 * 1024, std::fs::File::open(history_path)?);

    let mut file_writer: Option<BufWriter<std::fs::File>> = None;
    let mut stdout_writer: Option<BufWriter<std::io::StdoutLock<'_>>> = None;

    if let Some(p) = out_path.filter(|&p| p != "-") {
        if let Some(parent) = Path::new(p).parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(p)?;
        file_writer = Some(BufWriter::with_capacity(64 * 1024, f));
    } else {
        stdout_writer = Some(BufWriter::with_capacity(
            64 * 1024,
            std::io::stdout().lock(),
        ));
    }

    if scope == crate::arena::Scope::Global {
        let mut buf = vec![0u8; 64 * 1024];
        if let Some(ref mut w) = file_writer {
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
            }
            w.flush()?;
        } else if let Some(ref mut w) = stdout_writer {
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n])?;
            }
            w.flush()?;
        }
        return Ok(());
    }

    let mut line = Vec::new();
    let mut unesc = Vec::new();
    while reader.read_until(b'\n', &mut line)? > 0 {
        let keep = cwd.as_ref().is_none_or(|dir| {
            let mut parts = line.splitn(5, |&b| b == b'\t');
            let (_, _, _, row_cwd) = (parts.next(), parts.next(), parts.next(), parts.next());
            row_cwd.is_some_and(|c| {
                unescape_into(c, &mut unesc);
                unesc == dir.as_bytes()
            })
        });
        if keep {
            if let Some(ref mut w) = file_writer {
                w.write_all(&line)?;
            } else if let Some(ref mut w) = stdout_writer {
                w.write_all(&line)?;
            }
        }
        line.clear();
    }

    if let Some(mut w) = file_writer {
        w.flush()?;
    } else if let Some(mut w) = stdout_writer {
        w.flush()?;
    }
    Ok(())
}

/// Import shell history into xstory.
///
/// # Errors
/// Returns `Error::Io` or `Error::InvalidArgs` on failure.
pub fn import_history(
    in_path: Option<&str>,
    config: &Config,
) -> Result<usize, crate::error::Error> {
    import_history_to_path(in_path, config, &config.history_path)
}

struct IngestCtx<'a> {
    config: &'a Config,
    seen: HashSet<u64>,
    out_buf: Vec<u8>,
    writer: BufWriter<std::fs::File>,
    count: usize,
}

impl IngestCtx<'_> {
    fn push(
        &mut self,
        ts_ms: i64,
        dur_ms: u64,
        exit: i32,
        cwd: &str,
        raw_cmd: &str,
    ) -> std::io::Result<()> {
        let masked;
        let final_cmd = match evaluate(raw_cmd, self.config) {
            Verdict::Deny => return Ok(()),
            Verdict::Allow => raw_cmd,
            Verdict::Mask => {
                masked = mask_command(raw_cmd, self.config);
                &masked
            }
        };
        if !self.seen.insert(fnv1a(final_cmd.as_bytes())) {
            return Ok(());
        }
        let ts = if ts_ms > 0 { ts_ms } else { now_epoch_ms() };
        let _ = write!(self.out_buf, "{ts}\t{dur_ms}\t{exit}\t");
        escape_into(cwd, &mut self.out_buf);
        self.out_buf.push(b'\t');
        escape_into(final_cmd, &mut self.out_buf);
        self.out_buf.push(b'\n');
        self.writer.write_all(&self.out_buf)?;
        self.out_buf.clear();
        self.count += 1;
        Ok(())
    }

    fn push_tsv(&mut self, full: &[u8], unesc: &mut Vec<u8>) -> std::io::Result<()> {
        let mut p = full.splitn(5, |&b| b == b'\t');
        let ts = p
            .next()
            .and_then(|b| std::str::from_utf8(b).ok()?.parse().ok())
            .unwrap_or(0);
        let dur = p
            .next()
            .and_then(|b| std::str::from_utf8(b).ok()?.parse().ok())
            .unwrap_or(0);
        let exit = p
            .next()
            .and_then(|b| std::str::from_utf8(b).ok()?.parse().ok())
            .unwrap_or(0);
        let raw_dir = p.next().unwrap_or(b"?");
        let raw_command = p.next().unwrap_or(b"");
        unescape_into(raw_dir, unesc);
        let cwd = String::from_utf8_lossy(unesc).into_owned();
        unescape_into(raw_command, unesc);
        let cmd = String::from_utf8_lossy(unesc);
        self.push(ts, dur, exit, &cwd, &cmd)
    }
}

#[inline]
fn has_four_tabs(s: &[u8]) -> bool {
    let mut n = 0;
    for &b in s {
        if b == b'\t' {
            n += 1;
        }
    }
    n == 4
}

fn load_existing_hashes(target_path: &Path, unesc: &mut Vec<u8>) -> HashSet<u64> {
    let mut seen = HashSet::with_hasher(crate::arena::IdentityBuild::default());
    let mut raw_line: Vec<u8> = Vec::new();
    if let Ok(file) = std::fs::File::open(target_path) {
        let mut r = BufReader::with_capacity(64 * 1024, file);
        while r.read_until(b'\n', &mut raw_line).unwrap_or(0) > 0 {
            if let Some(cmd_raw) = raw_line.splitn(5, |&b| b == b'\t').nth(4) {
                let cmd_bytes = cmd_raw.strip_suffix(b"\n").unwrap_or(cmd_raw);
                let cmd_bytes = cmd_bytes.strip_suffix(b"\r").unwrap_or(cmd_bytes);
                unescape_into(cmd_bytes, unesc);
                seen.insert(fnv1a(unesc));
            }
            raw_line.clear();
        }
    }
    seen
}

fn open_input_stream(in_path: Option<&str>) -> Result<InStream, crate::error::Error> {
    match in_path {
        Some("-") => Ok(InStream::Stdin(BufReader::with_capacity(
            64 * 1024,
            std::io::stdin(),
        ))),
        Some(p) => {
            let path = if p == "~" {
                std::env::var_os("HOME").map_or_else(|| PathBuf::from(p), PathBuf::from)
            } else if let Some(stripped) = p.strip_prefix("~/") {
                std::env::var_os("HOME")
                    .map_or_else(|| PathBuf::from(p), |h| PathBuf::from(h).join(stripped))
            } else {
                PathBuf::from(p)
            };
            Ok(InStream::File(BufReader::with_capacity(
                64 * 1024,
                std::fs::File::open(path)?,
            )))
        }
        None => {
            let p = detect_shell_history().ok_or(crate::error::Error::InvalidArgs(
                "could not auto-detect shell history file",
            ))?;
            Ok(InStream::File(BufReader::with_capacity(
                64 * 1024,
                std::fs::File::open(p)?,
            )))
        }
    }
}

fn open_target_append(target_path: &Path) -> std::io::Result<BufWriter<std::fs::File>> {
    if let Some(parent) = target_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let target_file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(target_path)?;
    Ok(BufWriter::with_capacity(64 * 1024, target_file))
}

/// Import shell history to an explicit target path (for testing).
///
/// # Errors
/// Returns `Error::Io` or `Error::InvalidArgs` on failure.
#[allow(clippy::too_many_lines)]
pub fn import_history_to_path(
    in_path: Option<&str>,
    config: &Config,
    target_path: &Path,
) -> Result<usize, crate::error::Error> {
    let mut stream = open_input_stream(in_path)?;
    let mut unesc = Vec::new();
    let seen = load_existing_hashes(target_path, &mut unesc);
    let writer = open_target_append(target_path)?;

    let mut ctx = IngestCtx {
        config,
        seen,
        out_buf: Vec::with_capacity(256),
        writer,
        count: 0,
    };

    let mut raw_line: Vec<u8> = Vec::new();
    let mut multi_buf = Vec::new();
    let mut pending_ts: i64 = 0;
    let mut pending_fish: Option<String> = None;

    loop {
        raw_line.clear();
        if read_stream_line(&mut stream, &mut raw_line)? == 0 {
            break;
        }
        if raw_line.ends_with(b"\n") {
            raw_line.pop();
        }
        if raw_line.ends_with(b"\r") {
            raw_line.pop();
        }
        if raw_line.is_empty() && multi_buf.is_empty() {
            continue;
        }

        unmetafy_in_place(&mut raw_line);

        // 1. Fish YAML format (`- cmd: ...` / `  when: ...`)
        if multi_buf.is_empty() {
            if let Some(cmd_raw) = raw_line.strip_prefix(b"- cmd: ") {
                if let Some(prev) = pending_fish.take() {
                    ctx.push(pending_ts, 0, 0, "?", &prev)?;
                    pending_ts = 0;
                }
                unescape_into(cmd_raw, &mut unesc);
                pending_fish = Some(String::from_utf8_lossy(&unesc).into_owned());
                continue;
            } else if pending_fish.is_some() {
                if let Some(w) = raw_line.strip_prefix(b"  when: ") {
                    pending_ts = std::str::from_utf8(w)
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                        .unwrap_or(0)
                        * 1000;
                    continue;
                }
                // Discard file arguments (paths:) and other internal metadata
                if raw_line.starts_with(b"  ") {
                    continue;
                }
            }
        }

        // 2. Bash timestamp (`#1700000000`)
        if multi_buf.is_empty()
            && raw_line.starts_with(b"#")
            && (10..=14).contains(&raw_line.len())
            && raw_line[1..].iter().all(u8::is_ascii_digit)
        {
            pending_ts = std::str::from_utf8(&raw_line[1..])
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0)
                * 1000;
            continue;
        }

        // 3. Multiline backslash continuation (Zsh / Bash)
        if has_odd_trailing_backslashes(&raw_line) {
            raw_line.pop();
            multi_buf.extend_from_slice(&raw_line);
            multi_buf.push(b'\n');
            continue;
        }
        let full = if multi_buf.is_empty() {
            &raw_line[..]
        } else {
            multi_buf.extend_from_slice(&raw_line);
            &multi_buf[..]
        };

        // 4. Native 5-column TSV
        if full.first().is_some_and(u8::is_ascii_digit) && has_four_tabs(full) {
            ctx.push_tsv(full, &mut unesc)?;
            multi_buf.clear();
            continue;
        }

        // 5. Zsh Extended (`: <ts>:<dur>;<cmd>`) or Plain command
        if let Some(rest) = full.strip_prefix(b": ")
            && let Some(semi) = rest.iter().position(|&b| b == b';')
        {
            let header = &rest[..semi];
            let cmd = &rest[semi + 1..];
            let mut h = header.splitn(2, |&b| b == b':');
            let ts = h
                .next()
                .and_then(|s| std::str::from_utf8(s).ok()?.parse::<i64>().ok())
                .unwrap_or(0)
                * 1000;
            let dur = h
                .next()
                .and_then(|s| std::str::from_utf8(s).ok()?.parse::<u64>().ok())
                .unwrap_or(0)
                * 1000;
            ctx.push(ts, dur, 0, "?", &String::from_utf8_lossy(cmd))?;
            multi_buf.clear();
            continue;
        }

        ctx.push(pending_ts, 0, 0, "?", &String::from_utf8_lossy(full))?;
        pending_ts = 0;
        multi_buf.clear();
    }

    if let Some(prev) = pending_fish.take() {
        ctx.push(pending_ts, 0, 0, "?", &prev)?;
    }

    ctx.writer.flush()?;
    Ok(ctx.count)
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_config(path: PathBuf) -> Config {
        Config {
            history_path: path,
            scroll_off: 4,
            preview_mode: crate::arena::PreviewMode::Fixed,
            theme: None,
            exclude_commands: vec!["exit".into(), "logout".into(), "clear".into()],
            secret_patterns: vec![
                "AWS_SECRET".into(),
                "GITHUB_TOKEN".into(),
                "--password".into(),
            ],
        }
    }

    #[test]
    fn empty_command_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&[String::new()], 0, 100, Some("/home"), &cfg, &path).expect("record");
        assert!(!path.exists() || std::fs::read_to_string(&path).expect("read").is_empty());
    }

    #[test]
    fn whitespace_command_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&["   ".into()], 0, 100, Some("/home"), &cfg, &path).expect("record");
        assert!(!path.exists() || std::fs::read_to_string(&path).expect("read").is_empty());
    }

    #[test]
    fn ignorespace_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&[" ls -la".into()], 0, 100, Some("/home"), &cfg, &path).expect("record");
        assert!(!path.exists() || std::fs::read_to_string(&path).expect("read").is_empty());
    }

    #[test]
    fn excluded_command_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&["exit".into()], 0, 100, Some("/home"), &cfg, &path).expect("record");
        assert!(!path.exists() || std::fs::read_to_string(&path).expect("read").is_empty());

        record_to_path(&["logout".into()], 0, 100, Some("/home"), &cfg, &path).expect("record");
        assert!(!path.exists() || std::fs::read_to_string(&path).expect("read").is_empty());
    }

    #[test]
    fn secret_patterns_masked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(
            &["export".into(), "AWS_SECRET_KEY=foo123".into()],
            0,
            100,
            Some("/home"),
            &cfg,
            &path,
        )
        .expect("record");

        let content = std::fs::read_to_string(&path).expect("read");
        assert!(!content.contains("AWS_SECRET_KEY=foo123"));
        assert!(content.contains("***"));
    }

    #[test]
    fn special_chars_escaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(
            &["echo".into(), "hello\tworld".into()],
            0,
            100,
            Some("/home"),
            &cfg,
            &path,
        )
        .expect("record");

        let content = std::fs::read_to_string(&path).expect("read");
        // Tabs in command should be escaped as \t literal
        assert!(content.contains("hello\\tworld"));
    }

    #[test]
    fn newline_escaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(
            &["echo".into(), "line1\nline2".into()],
            0,
            100,
            Some("/home"),
            &cfg,
            &path,
        )
        .expect("record");

        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("line1\\nline2"));
    }

    #[test]
    fn backslash_escaped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(
            &["echo".into(), "path\\dir".into()],
            0,
            100,
            Some("/home"),
            &cfg,
            &path,
        )
        .expect("record");

        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("path\\\\dir"));
    }

    #[test]
    fn file_permissions() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&["ls".into()], 0, 100, Some("/home"), &cfg, &path).expect("record");

        let meta = std::fs::metadata(&path).expect("metadata");
        let mode = meta.mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn roundtrip_record_then_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(
            &["git".into(), "status".into()],
            0,
            250,
            Some("/home/user"),
            &cfg,
            &path,
        )
        .expect("record");

        let arena = crate::arena::Arena::load(&path, None).expect("load");
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.cmd_str(0), "git status");
        assert_eq!(arena.cwd_str(0), "/home/user");
        assert_eq!(arena.exit_codes[0], 0);
        assert_eq!(arena.durations[0], 250);
    }

    #[test]
    fn multiple_records_append() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&["ls".into()], 0, 10, Some("/home"), &cfg, &path).expect("r1");
        record_to_path(&["pwd".into()], 0, 5, Some("/tmp"), &cfg, &path).expect("r2");
        record_to_path(&["whoami".into()], 0, 3, Some("/home"), &cfg, &path).expect("r3");

        let arena = crate::arena::Arena::load(&path, None).expect("load");
        assert_eq!(arena.len(), 3);
        assert_eq!(arena.cmd_str(0), "ls");
        assert_eq!(arena.cmd_str(1), "pwd");
        assert_eq!(arena.cmd_str(2), "whoami");
    }

    #[test]
    fn valid_command_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(
            &["ls".into(), "-la".into()],
            0,
            100,
            Some("/home"),
            &cfg,
            &path,
        )
        .expect("record");

        let content = std::fs::read_to_string(&path).expect("read");
        assert!(!content.is_empty());
        // Verify TSV has 5 tab-separated fields
        let line = content.trim();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[3], "/home");
        assert_eq!(fields[4], "ls -la");
    }

    #[test]
    fn unmetafy_zsh_utf8_cyrillic() {
        let mut buf = vec![0x83, 0xf0, 0x83, 0x90];
        unmetafy_in_place(&mut buf);
        assert_eq!(buf, vec![0xd0, 0xb0]);
        assert_eq!(std::str::from_utf8(&buf).expect("utf8"), "а");
    }

    #[test]
    fn import_zsh_extended_and_folding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hist = dir.path().join("history.tsv");
        let src = dir.path().join("zsh_src");
        let content = ": 1700000000:3;git commit -m \\\n\"first commit\"\n: 1700000010:0;ls -la\n";
        std::fs::write(&src, content).expect("write");

        let cfg = test_config(hist.clone());
        let count =
            import_history_to_path(Some(src.to_str().expect("path")), &cfg, &hist).expect("import");
        assert_eq!(count, 2);

        let arena = crate::arena::Arena::load(&hist, None).expect("load");
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.cmd_str(0), "git commit -m \n\"first commit\"");
        assert_eq!(arena.timestamps[0], 1_700_000_000_000);
        assert_eq!(arena.durations[0], 3000);
        assert_eq!(arena.cmd_str(1), "ls -la");
    }

    #[test]
    fn import_bash_timestamps_and_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hist = dir.path().join("history.tsv");
        let src = dir.path().join("bash_src");
        let content = "#1700000000\ncargo build --release\n# this is a comment\npwd\n";
        std::fs::write(&src, content).expect("write");

        let cfg = test_config(hist.clone());
        let count =
            import_history_to_path(Some(src.to_str().expect("path")), &cfg, &hist).expect("import");
        assert_eq!(count, 3);

        let arena = crate::arena::Arena::load(&hist, None).expect("load");
        assert_eq!(arena.len(), 3);
        assert_eq!(arena.cmd_str(0), "cargo build --release");
        assert_eq!(arena.timestamps[0], 1_700_000_000_000);
        assert_eq!(arena.cmd_str(1), "# this is a comment");
        assert_eq!(arena.cmd_str(2), "pwd");
    }

    #[test]
    fn import_fish_yaml_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hist = dir.path().join("history.tsv");
        let src = dir.path().join("fish_src");
        let content = "- cmd: echo \"hello\\nworld\"\n  when: 1700000000\n  paths:\n    - /tmp\n- cmd: cargo test\n  when: 1700000010\n";
        std::fs::write(&src, content).expect("write");

        let cfg = test_config(hist.clone());
        let count =
            import_history_to_path(Some(src.to_str().expect("path")), &cfg, &hist).expect("import");
        assert_eq!(count, 2);

        let arena = crate::arena::Arena::load(&hist, None).expect("load");
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.cmd_str(0), "echo \"hello\nworld\"");
        assert_eq!(arena.cwd_str(0), "?");
        assert_eq!(arena.timestamps[0], 1_700_000_000_000);
        assert_eq!(arena.cmd_str(1), "cargo test");
    }

    #[test]
    fn import_deduplication_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hist = dir.path().join("history.tsv");
        let src = dir.path().join("dups.log");
        let content = "git status\ngit status\ncargo check\ngit status\n";
        std::fs::write(&src, content).expect("write");

        let cfg = test_config(hist.clone());
        let count =
            import_history_to_path(Some(src.to_str().expect("path")), &cfg, &hist).expect("import");
        assert_eq!(count, 2);

        let count2 = import_history_to_path(Some(src.to_str().expect("path")), &cfg, &hist)
            .expect("import2");
        assert_eq!(count2, 0);
    }

    #[test]
    fn export_history_roundtrip_and_permissions() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let hist = dir.path().join("history.tsv");
        let exported = dir.path().join("export.tsv");
        let cfg = test_config(hist.clone());

        record_to_path(
            &["echo".into(), "hello".into()],
            0,
            50,
            Some("/tmp"),
            &cfg,
            &hist,
        )
        .expect("rec");
        export_history_from_path(
            Some(exported.to_str().expect("path")),
            crate::arena::Scope::Global,
            &hist,
        )
        .expect("export");

        let meta = std::fs::metadata(&exported).expect("meta");
        assert_eq!(meta.mode() & 0o777, 0o600);

        let content = std::fs::read_to_string(&exported).expect("read");
        assert!(content.contains("echo hello"));
    }
}
