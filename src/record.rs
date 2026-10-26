//! Ingestion pipeline: filter → serialize → atomic append.
//! Hot path: ≤1 allocation. Borrows all inputs.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::arena::Config;

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
            b'\t' => { buf.extend_from_slice(b"\\t"); escaped = true; }
            b'\n' => { buf.extend_from_slice(b"\\n"); escaped = true; }
            b'\\' => { buf.extend_from_slice(b"\\\\"); escaped = true; }
            _ => buf.push(b),
        }
    }
    escaped
}

// ── Policy ───────────────────────────────────────────────────────

/// Check if command should be recorded, denied, or masked.
fn evaluate(cmd: &str, config: &Config) -> Verdict {
    // Empty or whitespace-only → deny
    if cmd.trim().is_empty() {
        return Verdict::Deny;
    }

    // Ignorespace: leading space → deny (bash HISTCONTROL=ignorespace)
    if cmd.starts_with(' ') {
        return Verdict::Deny;
    }

    // Excluded commands: exact match against trimmed command base
    let base = cmd.split_whitespace().next().unwrap_or("");
    for excl in &config.exclude_commands {
        if base == excl {
            return Verdict::Deny;
        }
    }

    // Secret patterns: substring match (not regex — O(1) amortized)
    for pat in &config.secret_patterns {
        if cmd.contains(pat.as_str()) {
            return Verdict::Mask;
        }
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
                let val_end = val_rest.find(|c: char| c.is_whitespace()).unwrap_or(val_rest.len());
                1 + val_end
            } else if rest.starts_with(' ') {
                let trimmed = rest.trim_start();
                let spaces = rest.len() - trimmed.len();
                let val_end = trimmed.find(|c: char| c.is_whitespace()).unwrap_or(trimmed.len());
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
    let cmd = cmd_parts.join(" ");

    let (final_cmd, is_masked) = match evaluate(&cmd, config) {
        Verdict::Deny => return Ok(()),
        Verdict::Allow => (cmd, false),
        Verdict::Mask => (mask_command(&cmd, config), true),
    };
    let _ = is_masked;

    let cwd = cwd
        .or_else(|| std::env::var("PWD").ok().as_deref().map(|_| "?"))
        .unwrap_or("?");
    let actual_cwd = std::env::var("PWD").unwrap_or_else(|_| cwd.to_owned());
    let cwd_ref = if cwd == "?" { &actual_cwd } else { cwd };

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));

    // Pre-allocate: generous capacity to avoid reallocs
    let mut buf = Vec::with_capacity(64 + final_cmd.len() + cwd_ref.len());

    // timestamp
    write_i64(&mut buf, timestamp_ms);
    buf.push(b'\t');

    // duration
    write_u64(&mut buf, duration_ms);
    buf.push(b'\t');

    // exit code
    write_i32(&mut buf, exit_code);
    buf.push(b'\t');

    // cwd (escaped)
    escape_into(cwd_ref, &mut buf);
    buf.push(b'\t');

    // command (escaped)
    escape_into(&final_cmd, &mut buf);
    buf.push(b'\n');

    // Ensure parent directory exists
    if let Some(parent) = config.history_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Atomic append: O_APPEND, mode 0o600
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&config.history_path)?;

    file.write_all(&buf)?;

    Ok(())
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

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));

    let mut buf = Vec::with_capacity(64 + final_cmd.len() + cwd_ref.len());

    write_i64(&mut buf, timestamp_ms);
    buf.push(b'\t');
    write_u64(&mut buf, duration_ms);
    buf.push(b'\t');
    write_i32(&mut buf, exit_code);
    buf.push(b'\t');
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

// ── Integer serialization (no alloc) ─────────────────────────────

fn write_i64(buf: &mut Vec<u8>, val: i64) {
    if val < 0 {
        buf.push(b'-');
        write_u64(buf, val.unsigned_abs());
    } else {
        #[allow(clippy::cast_sign_loss)]
        write_u64(buf, val as u64);
    }
}

#[allow(clippy::cast_possible_truncation)]
fn write_u64(buf: &mut Vec<u8>, val: u64) {
    if val == 0 {
        buf.push(b'0');
        return;
    }
    let start = buf.len();
    let mut v = val;
    while v > 0 {
        buf.push(b'0' + (v % 10) as u8);
        v /= 10;
    }
    buf[start..].reverse();
}

fn write_i32(buf: &mut Vec<u8>, val: i32) {
    if val < 0 {
        buf.push(b'-');
        write_u64(buf, u64::from(val.unsigned_abs()));
    } else {
        #[allow(clippy::cast_sign_loss)]
        write_u64(buf, val as u64);
    }
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
            exclude_commands: vec!["exit".into(), "logout".into(), "clear".into()],
            secret_patterns: vec!["AWS_SECRET".into(), "GITHUB_TOKEN".into(), "--password".into()],
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
            0, 100, Some("/home"), &cfg, &path,
        ).expect("record");

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
            0, 100, Some("/home"), &cfg, &path,
        ).expect("record");

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
            0, 100, Some("/home"), &cfg, &path,
        ).expect("record");

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
            0, 100, Some("/home"), &cfg, &path,
        ).expect("record");

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
            0, 250, Some("/home/user"), &cfg, &path,
        ).expect("record");

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
    fn write_i64_formatting() {
        let mut buf = Vec::new();
        write_i64(&mut buf, 12345);
        assert_eq!(std::str::from_utf8(&buf).expect("utf8"), "12345");

        buf.clear();
        write_i64(&mut buf, -42);
        assert_eq!(std::str::from_utf8(&buf).expect("utf8"), "-42");

        buf.clear();
        write_i64(&mut buf, 0);
        assert_eq!(std::str::from_utf8(&buf).expect("utf8"), "0");
    }

    #[test]
    fn valid_command_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let cfg = test_config(path.clone());

        record_to_path(&["ls".into(), "-la".into()], 0, 100, Some("/home"), &cfg, &path).expect("record");

        let content = std::fs::read_to_string(&path).expect("read");
        assert!(!content.is_empty());
        // Verify TSV has 5 tab-separated fields
        let line = content.trim();
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[3], "/home");
        assert_eq!(fields[4], "ls -la");
    }
}
