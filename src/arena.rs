#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use memmap2::Mmap;

// ── Core Data Types ──────────────────────────────────────────────

/// Cache-aligned string descriptor. 16 bytes, fits 4 per L1 cache line.
/// Encodes inline (≤12 bytes) vs out-of-line (>12 bytes) in its repr:
/// - Inline: prefix\[0..4\] ++ data\[0..len-4\], contiguous in `repr(C)`.
/// - Out-of-line: prefix\[0..4\] is the first 4 bytes (for display),
///   data = \[`buffer_id`: u32 LE, offset: u32 LE\].
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct ByteView {
    pub len:    u32,
    pub prefix: [u8; 4],
    pub data:   [u8; 8],
}

/// Candidate scored for Top-K heap. 8 bytes, Copy.
/// Custom Ord: REVERSE score order so `BinaryHeap` (max-heap)
/// keeps the MINIMUM score at `peek()` for O(1) rejection.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Scored {
    pub score: u32,
    pub idx:   u32,
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.score.cmp(&self.score)
            .then_with(|| other.idx.cmp(&self.idx))
    }
}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Scope filter for local vs global search.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Global,
    Local,
}

/// Terminal color scheme.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    Dark,
    Light,
}

/// Application configuration.
///
/// Loaded once at startup via `Config::load()`.
/// File format: plain `key = value` lines, `#` comments, blank lines ignored.
/// Env vars override file values. Compiled-in defaults as final fallback.
pub struct Config {
    pub history_path:     PathBuf,
    pub scroll_off:       u16,
    pub exclude_commands: Vec<String>,
    pub secret_patterns:  Vec<String>,
}

// ── Arena ────────────────────────────────────────────────────────

/// Columnar in-memory arena. Owns the mmap and all decoded columns.
/// Lifetime-free: indices and `ByteView` point into `mmap` or `unescape_buf`.
pub struct Arena {
    // owned backing buffers
    mmap:          Option<Mmap>,
    unescape_buf:  Vec<u8>,

    // parallel column vectors (raw, including duplicates)
    pub timestamps:  Vec<i64>,
    pub durations:   Vec<u32>,
    pub exit_codes:  Vec<i32>,
    pub commands:    Vec<ByteView>,
    pub cwds:        Vec<ByteView>,
    pub masks:       Vec<u64>,

    // deduplicated scope indices (precomputed in load)
    pub global_indices: Vec<u32>,
    pub local_indices:  Vec<u32>,
}

// ── FNV-1a ───────────────────────────────────────────────────────

#[must_use]
#[inline]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_0100_01b3);
    }
    h
}

// ── Inline atoi (no alloc) ───────────────────────────────────────

/// Parse i64 from ASCII bytes. Returns None on invalid input.
fn parse_i64(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = if s[0] == b'-' {
        (true, &s[1..])
    } else {
        (false, s)
    };
    if digits.is_empty() {
        return None;
    }
    let mut val: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    if neg { Some(-val) } else { Some(val) }
}

/// Parse u32 from ASCII bytes. Returns None on invalid input.
fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut val: u32 = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(val)
}

/// Parse i32 from ASCII bytes. Returns None on invalid input.
fn parse_i32(s: &[u8]) -> Option<i32> {
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = if s[0] == b'-' {
        (true, &s[1..])
    } else {
        (false, s)
    };
    if digits.is_empty() {
        return None;
    }
    let mut val: i32 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(i32::from(b - b'0'))?;
    }
    if neg { Some(-val) } else { Some(val) }
}

// ── ByteView construction ────────────────────────────────────────

/// Build a `ByteView` for a byte slice that requires no unescaping.
/// If `len ≤ 12` → inline. Otherwise → out-of-line pointing into mmap (buffer 0).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn byteview_raw(s: &[u8], mmap_base: *const u8) -> ByteView {
    let len = s.len();
    let mut bv = ByteView {
        len: len as u32,
        prefix: [0; 4],
        data: [0; 8],
    };

    if len <= 12 {
        let copy_prefix = len.min(4);
        bv.prefix[..copy_prefix].copy_from_slice(&s[..copy_prefix]);
        if len > 4 {
            let tail = len - 4;
            bv.data[..tail].copy_from_slice(&s[4..]);
        }
    } else {
        bv.prefix[..4].copy_from_slice(&s[..4]);
        // SAFETY: s points into the mmap, mmap_base is the start of the mmap.
        // The offset is always positive and within u32 range for files < 4 GiB.
        let offset = unsafe { s.as_ptr().offset_from(mmap_base) } as u32;
        bv.data[..4].copy_from_slice(&0_u32.to_le_bytes());
        bv.data[4..8].copy_from_slice(&offset.to_le_bytes());
    }
    bv
}

/// Build a `ByteView` for an unescaped byte slice.
/// If `len ≤ 12` → inlined from the unescaped data.
/// Otherwise → out-of-line, `buffer_id` = 1.
#[allow(clippy::cast_possible_truncation)]
fn byteview_unescaped(s: &[u8], offset_in_buf: u32) -> ByteView {
    let len = s.len();
    let mut bv = ByteView {
        len: len as u32,
        prefix: [0; 4],
        data: [0; 8],
    };
    let copy_prefix = len.min(4);
    bv.prefix[..copy_prefix].copy_from_slice(&s[..copy_prefix]);

    if len <= 12 {
        if len > 4 {
            let tail = len - 4;
            bv.data[..tail].copy_from_slice(&s[4..]);
        }
    } else {
        bv.data[..4].copy_from_slice(&1_u32.to_le_bytes());
        bv.data[4..8].copy_from_slice(&offset_in_buf.to_le_bytes());
    }
    bv
}

// ── Unescape ─────────────────────────────────────────────────────

/// Check if a byte slice contains any escape sequences (`\\`).
#[inline]
fn has_escapes(s: &[u8]) -> bool {
    memchr::memchr(b'\\', s).is_some()
}

/// Unescape `\\t`, `\\n`, `\\\\` into `buf`.
/// Returns (`start_offset`, length) of the unescaped data in `buf`.
#[allow(clippy::cast_possible_truncation)]
fn unescape_into(s: &[u8], buf: &mut Vec<u8>) -> (u32, usize) {
    let start = buf.len() as u32;
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 1 < s.len() {
            match s[i + 1] {
                b't'  => buf.push(b'\t'),
                b'n'  => buf.push(b'\n'),
                b'\\' => buf.push(b'\\'),
                other => {
                    buf.push(b'\\');
                    buf.push(other);
                }
            }
            i += 2;
        } else {
            buf.push(s[i]);
            i += 1;
        }
    }
    let len = buf.len() - start as usize;
    (start, len)
}

// ── Char bitmask ─────────────────────────────────────────────────

/// Compute 64-bit character bitmask for fast-reject filtering.
/// Each byte maps to one of 64 bits via `b & 63`.
#[must_use]
pub fn char_mask(s: &[u8]) -> u64 {
    let mut mask: u64 = 0;
    for &b in s {
        mask |= 1_u64 << (b & 63);
    }
    mask
}

// ── Arena implementation ─────────────────────────────────────────

impl Arena {
    /// Load history from file path. Single allocation boundary.
    /// Returns Arena with all columns populated, corrupted lines silently skipped.
    /// If the file is missing or empty, returns an empty Arena (not an error).
    ///
    /// # Errors
    /// Returns `Error::Io` if opening or mapping the file fails.
    #[allow(clippy::similar_names, clippy::cast_possible_truncation)]
    pub fn load(path: &Path, local_cwd: Option<&str>) -> Result<Self, crate::error::Error> {
        let mut arena = Self {
            mmap: None,
            unescape_buf: Vec::new(),
            timestamps: Vec::new(),
            durations: Vec::new(),
            exit_codes: Vec::new(),
            commands: Vec::new(),
            cwds: Vec::new(),
            masks: Vec::new(),
            global_indices: Vec::new(),
            local_indices: Vec::new(),
        };

        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(arena),
            Err(e) => return Err(e.into()),
        };

        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            return Ok(arena);
        }

        // SAFETY: file opened read-only, not modified during our lifetime.
        let mmap = unsafe { Mmap::map(&file)? };
        let mmap_base = mmap.as_ptr();
        let data = &mmap[..];

        let line_count = memchr::memchr_iter(b'\n', data).count();
        arena.timestamps.reserve(line_count);
        arena.durations.reserve(line_count);
        arena.exit_codes.reserve(line_count);
        arena.commands.reserve(line_count);
        arena.cwds.reserve(line_count);
        arena.masks.reserve(line_count);

        let mut pos = 0;
        while pos < data.len() {
            let line_end = memchr::memchr(b'\n', &data[pos..])
                .map_or(data.len(), |i| pos + i);
            let line = &data[pos..line_end];
            pos = line_end + 1;

            if line.is_empty() {
                continue;
            }

            // 5 fields: timestamp \t duration \t exit_code \t cwd \t command
            let Some(t1) = memchr::memchr(b'\t', line) else { continue };
            let Some(t2) = memchr::memchr(b'\t', &line[t1 + 1..]).map(|i| t1 + 1 + i) else { continue };
            let Some(t3) = memchr::memchr(b'\t', &line[t2 + 1..]).map(|i| t2 + 1 + i) else { continue };
            let Some(t4) = memchr::memchr(b'\t', &line[t3 + 1..]).map(|i| t3 + 1 + i) else { continue };
            if memchr::memchr(b'\t', &line[t4 + 1..]).is_some() {
                continue; // >5 fields → malformed
            }

            let field_ts  = &line[..t1];
            let field_dur = &line[t1 + 1..t2];
            let field_exit = &line[t2 + 1..t3];
            let field_cwd = &line[t3 + 1..t4];
            let field_cmd = &line[t4 + 1..];

            let Some(ts)   = parse_i64(field_ts)   else { continue };
            let Some(dur)  = parse_u32(field_dur)   else { continue };
            let Some(exit) = parse_i32(field_exit)  else { continue };

            let bv_cwd = if has_escapes(field_cwd) {
                let (offset, len) = unescape_into(field_cwd, &mut arena.unescape_buf);
                let unescaped = &arena.unescape_buf[offset as usize..offset as usize + len];
                byteview_unescaped(unescaped, offset)
            } else {
                byteview_raw(field_cwd, mmap_base)
            };

            let cmd_has_esc = has_escapes(field_cmd);
            let cmd_unescape_start = arena.unescape_buf.len();
            let bv_cmd = if cmd_has_esc {
                let (offset, len) = unescape_into(field_cmd, &mut arena.unescape_buf);
                let unescaped = &arena.unescape_buf[offset as usize..offset as usize + len];
                byteview_unescaped(unescaped, offset)
            } else {
                byteview_raw(field_cmd, mmap_base)
            };

            let mask = char_mask(if cmd_has_esc {
                // Resolve from unescape_buf; start recorded before the push, length from ByteView
                debug_assert!(arena.unescape_buf.len() >= cmd_unescape_start + bv_cmd.len as usize);
                &arena.unescape_buf[cmd_unescape_start..cmd_unescape_start + bv_cmd.len as usize]
            } else {
                field_cmd
            });

            arena.timestamps.push(ts);
            arena.durations.push(dur);
            arena.exit_codes.push(exit);
            arena.cwds.push(bv_cwd);
            arena.commands.push(bv_cmd);
            arena.masks.push(mask);
        }

        arena.mmap = Some(mmap);
        arena.build_dedup_indices(local_cwd);

        Ok(arena)
    }

    /// Build deduplicated index vectors via reverse-chronological FNV-1a scan.
    fn build_dedup_indices(&mut self, local_cwd: Option<&str>) {
        let n = self.commands.len();
        if n == 0 {
            return;
        }

        // HashMap keyed by FNV-1a hash → Vec of indices with that hash.
        // Collision resolution: O(k) per bucket where k = collision count (typically 1).
        let mut seen: HashMap<u64, Vec<u32>> = HashMap::with_capacity(n);
        let mut global_buf: Vec<u32> = Vec::with_capacity(n);
        let mut local_buf: Vec<u32> = Vec::with_capacity(n / 4);

        for raw_idx in (0..n).rev() {
            #[allow(clippy::cast_possible_truncation)]
            let idx = raw_idx as u32;
            let cmd = self.cmd_bytes(idx);
            let h = fnv1a(cmd);

            if let Some(indices) = seen.get(&h)
                && indices.iter().any(|&si| self.cmd_bytes(si) == cmd)
            {
                continue;
            }

            seen.entry(h).or_default().push(idx);
            global_buf.push(idx);

            if let Some(lcwd) = local_cwd
                && self.cwd_bytes(idx) == lcwd.as_bytes()
            {
                local_buf.push(idx);
            }
        }

        // Walk already produces newest-first order (reverse chronological).
        // No reversal needed.

        self.global_indices = global_buf;
        self.local_indices = local_buf;
    }

    /// Resolve a command `ByteView` by column index to the actual byte slice.
    #[must_use]
    pub fn cmd_bytes(&self, idx: u32) -> &[u8] {
        self.resolve_bv(&self.commands[idx as usize])
    }

    /// Resolve a cwd `ByteView` by column index to the actual byte slice.
    #[must_use]
    pub fn cwd_bytes(&self, idx: u32) -> &[u8] {
        self.resolve_bv(&self.cwds[idx as usize])
    }

    /// Core resolver: takes `&ByteView` (borrowed from `self.commands`/`cwds` Vec),
    /// so inlined bytes live as long as `&self`.
    fn resolve_bv(&self, bv: &ByteView) -> &[u8] {
        let len = bv.len as usize;
        if len <= 12 {
            // SAFETY: bv is borrowed from self.commands/cwds Vec, lives as long as &self.
            // prefix and data are contiguous in repr(C, align(16)) layout.
            unsafe {
                let ptr = bv.prefix.as_ptr();
                std::slice::from_raw_parts(ptr, len)
            }
        } else {
            let buffer_id = u32::from_le_bytes([bv.data[0], bv.data[1], bv.data[2], bv.data[3]]);
            let offset    = u32::from_le_bytes([bv.data[4], bv.data[5], bv.data[6], bv.data[7]]);
            let buf = match (buffer_id, &self.mmap) {
                (0, Some(m)) => &m[..],
                _            => &self.unescape_buf[..],
            };
            &buf[offset as usize..][..len]
        }
    }

    /// Resolve command at index → `&str` (unchecked UTF-8, validated at parse time).
    #[must_use]
    #[inline]
    pub fn cmd_str(&self, idx: u32) -> &str {
        // SAFETY: TSV parse validates UTF-8 at ingestion boundary.
        unsafe { std::str::from_utf8_unchecked(self.cmd_bytes(idx)) }
    }

    /// Resolve cwd at index → `&str`.
    #[must_use]
    #[inline]
    pub fn cwd_str(&self, idx: u32) -> &str {
        // SAFETY: TSV parse validates UTF-8 at ingestion boundary.
        unsafe { std::str::from_utf8_unchecked(self.cwd_bytes(idx)) }
    }

    /// Get the deduplicated indices slice for the active scope.
    #[must_use]
    pub fn scope_indices(&self, scope: Scope) -> &[u32] {
        match scope {
            Scope::Global => &self.global_indices,
            Scope::Local  => &self.local_indices,
        }
    }

    /// Total raw entry count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the arena has no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

// ── Config ───────────────────────────────────────────────────────

impl Config {
    /// Load config: env vars → `~/.config/xstory/config` → compiled-in defaults.
    #[must_use]
    pub fn load() -> Self {
        let mut cfg = Self::defaults();

        if let Some(config_path) = Self::config_file_path()
            && let Ok(contents) = std::fs::read_to_string(&config_path)
        {
            Self::parse_config_file(&contents, &mut cfg);
        }

        if let Ok(val) = std::env::var("XSTORY_HISTORY_PATH") {
            cfg.history_path = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("XSTORY_SCROLL_OFF")
            && let Ok(n) = val.parse::<u16>()
        {
            cfg.scroll_off = n;
        }

        cfg
    }

    fn defaults() -> Self {
        let history_path = std::env::var("HOME").map_or_else(
            |_| PathBuf::from("/tmp/xstory_history.tsv"),
            |h| PathBuf::from(h).join(".local/share/xstory/history.tsv"),
        );

        Self {
            history_path,
            scroll_off: 4,
            exclude_commands: vec![
                "exit".into(),
                "logout".into(),
                "clear".into(),
            ],
            secret_patterns: vec![
                "AWS_SECRET".into(),
                "GITHUB_TOKEN".into(),
                "--password".into(),
            ],
        }
    }

    fn config_file_path() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config/xstory/config"))
    }

    fn parse_config_file(contents: &str, cfg: &mut Self) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else { continue };
            let key = key.trim();
            let val = val.trim();

            match key {
                "history_path" => cfg.history_path = PathBuf::from(val),
                "scroll_off" => {
                    if let Ok(n) = val.parse::<u16>() {
                        cfg.scroll_off = n;
                    }
                }
                "exclude_commands" => {
                    cfg.exclude_commands = val.split(',').map(|s| s.trim().to_owned()).collect();
                }
                "secret_patterns" => {
                    cfg.secret_patterns = val.split(',').map(|s| s.trim().to_owned()).collect();
                }
                _ => {}
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tsv(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.tsv");
        let mut f = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        (dir, path)
    }

    #[test]
    fn roundtrip_basic() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tls -la",
            "2000\t100\t1\t/tmp\tgit status",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.timestamps[0], 1000);
        assert_eq!(arena.timestamps[1], 2000);
        assert_eq!(arena.durations[0], 50);
        assert_eq!(arena.durations[1], 100);
        assert_eq!(arena.exit_codes[0], 0);
        assert_eq!(arena.exit_codes[1], 1);
        assert_eq!(arena.cmd_str(0), "ls -la");
        assert_eq!(arena.cmd_str(1), "git status");
        assert_eq!(arena.cwd_str(0), "/home");
        assert_eq!(arena.cwd_str(1), "/tmp");
    }

    #[test]
    fn corrupted_lines_skipped() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tls",
            "bad line no tabs",
            "abc\t50\t0\t/home\tgit",
            "3000\t50\t0\t/home\tpwd",
            "4000\t50\t0\t/a\t/b\textra_tab",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.cmd_str(0), "ls");
        assert_eq!(arena.cmd_str(1), "pwd");
    }

    #[test]
    fn missing_file_empty_arena() {
        let arena = Arena::load(Path::new("/nonexistent/xstory_test_no_file"), None).expect("load");
        assert!(arena.is_empty());
        assert!(arena.global_indices.is_empty());
    }

    #[test]
    fn empty_file_empty_arena() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.tsv");
        std::fs::File::create(&path).expect("create");
        let arena = Arena::load(&path, None).expect("load");
        assert!(arena.is_empty());
    }

    #[test]
    fn escape_tab() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\techo hello\\tworld",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), "echo hello\tworld");
    }

    #[test]
    fn escape_newline() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\techo\\nhello",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), "echo\nhello");
    }

    #[test]
    fn escape_backslash() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tpath\\\\dir",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), "path\\dir");
    }

    #[test]
    fn byteview_inline_short() {
        let (_dir, path) = write_tsv(&["1000\t50\t0\t/a\tls"]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), "ls");
        assert!(arena.commands[0].len <= 12);
    }

    #[test]
    fn byteview_inline_exactly_12() {
        let (_dir, path) = write_tsv(&["1000\t50\t0\t/a\t123456789012"]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), "123456789012");
        assert_eq!(arena.commands[0].len, 12);
    }

    #[test]
    fn byteview_outofline_13() {
        let (_dir, path) = write_tsv(&["1000\t50\t0\t/a\t1234567890123"]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), "1234567890123");
        assert_eq!(arena.commands[0].len, 13);
    }

    #[test]
    fn byteview_outofline_long() {
        let long_cmd = "x".repeat(200);
        let line = format!("1000\t50\t0\t/home\t{long_cmd}");
        let (_dir, path) = write_tsv(&[&line]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.cmd_str(0), long_cmd);
    }

    #[test]
    fn char_mask_superset() {
        let full = char_mask(b"abc");
        let sub = char_mask(b"ab");
        assert_eq!(sub & full, sub);
    }

    #[test]
    fn char_mask_fast_reject() {
        let query = char_mask(b"xyz");
        let cand = char_mask(b"abc");
        assert_ne!(query & cand, query);
    }

    #[test]
    fn dedup_newest_wins() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/home\tpwd",
            "3000\t50\t0\t/home\tls",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.len(), 3);
        assert_eq!(arena.global_indices.len(), 2);
        assert_eq!(arena.cmd_str(arena.global_indices[0]), "ls");
        assert_eq!(arena.global_indices[0], 2);
    }

    #[test]
    fn dedup_different_commands_preserved() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tcommand_alpha",
            "2000\t50\t0\t/home\tcommand_beta",
            "3000\t50\t0\t/home\tcommand_gamma",
        ]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.global_indices.len(), 3);
    }

    #[test]
    fn scope_local_filters_cwd() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/tmp\tpwd",
            "3000\t50\t0\t/home\tgit status",
        ]);
        let arena = Arena::load(&path, Some("/home")).expect("load");
        assert_eq!(arena.global_indices.len(), 3);
        assert_eq!(arena.local_indices.len(), 2);
        for &idx in &arena.local_indices {
            assert_eq!(arena.cwd_str(idx), "/home");
        }
    }

    #[test]
    fn scope_local_dedup() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/tmp\tls",
            "3000\t50\t0\t/home\tls",
        ]);
        let arena = Arena::load(&path, Some("/home")).expect("load");
        assert_eq!(arena.global_indices.len(), 1);
        assert_eq!(arena.global_indices[0], 2);
        assert_eq!(arena.local_indices.len(), 1);
        assert_eq!(arena.local_indices[0], 2);
    }

    #[test]
    fn scope_indices_dispatch() {
        let (_dir, path) = write_tsv(&[
            "1000\t50\t0\t/home\tls",
            "2000\t50\t0\t/tmp\tpwd",
        ]);
        let arena = Arena::load(&path, Some("/home")).expect("load");
        assert_eq!(arena.scope_indices(Scope::Global).len(), 2);
        assert_eq!(arena.scope_indices(Scope::Local).len(), 1);
    }

    #[test]
    fn negative_exit_code() {
        let (_dir, path) = write_tsv(&["1000\t50\t-1\t/home\tfail"]);
        let arena = Arena::load(&path, None).expect("load");
        assert_eq!(arena.exit_codes[0], -1);
    }

    #[test]
    fn scored_reverse_ordering() {
        use std::collections::BinaryHeap;
        let mut heap = BinaryHeap::new();
        heap.push(Scored { score: 100, idx: 0 });
        heap.push(Scored { score: 50, idx: 1 });
        heap.push(Scored { score: 200, idx: 2 });
        let top = heap.peek().expect("peek");
        assert_eq!(top.score, 50);
    }

    #[test]
    fn scored_tie_breaking() {
        use std::collections::BinaryHeap;
        let mut heap = BinaryHeap::new();
        heap.push(Scored { score: 100, idx: 5 });
        heap.push(Scored { score: 100, idx: 2 });
        heap.push(Scored { score: 100, idx: 8 });
        let top = heap.peek().expect("peek");
        assert_eq!(top.idx, 2);
    }

    #[test]
    fn config_env_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let custom = dir.path().join("custom.tsv");
        unsafe { std::env::set_var("XSTORY_HISTORY_PATH", &custom) };
        let cfg = Config::load();
        unsafe { std::env::remove_var("XSTORY_HISTORY_PATH") };
        assert_eq!(cfg.history_path, custom);
    }

    #[test]
    fn config_defaults() {
        unsafe {
            std::env::remove_var("XSTORY_HISTORY_PATH");
            std::env::remove_var("XSTORY_SCROLL_OFF");
        }
        let cfg = Config::load();
        assert_eq!(cfg.scroll_off, 4);
        assert!(cfg.exclude_commands.contains(&"exit".to_owned()));
    }

    #[test]
    fn config_parse_file() {
        let mut cfg = Config::defaults();
        Config::parse_config_file("# comment\nscroll_off = 8\nexclude_commands = cd, ls\n", &mut cfg);
        assert_eq!(cfg.scroll_off, 8);
        assert_eq!(cfg.exclude_commands, vec!["cd", "ls"]);
    }
}
