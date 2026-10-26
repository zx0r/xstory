# ARCHITECTURE.md — xstory Reference Architecture

> Clean-room design from SPEC.md. Zero legacy anchoring.

---

## 0. Design Principles Applied

| Principle | Manifestation |
|---|---|
| Concrete over Abstract | Zero traits in the entire crate. Polymorphism via `enum` only. |
| Flat modules | 7 files. Deepest path: `src/tui.rs`. No subdirectories. |
| Data-Oriented | Columnar arena (`Arena`) owns all data. Everything else borrows or indexes into it. |
| Parse, Don't Validate | `RawLine → Entry` is the sole parse boundary. `ByteView` encodes inline/out-of-line in its repr, not a bool flag. |
| Centralized control | `main()` dispatches CLI. `run_tui()` is a single `loop { match event }`. |
| Flat errors | One `enum Error` with `Display + From` impls. No macros, no crates. |

---

## 1. File Tree & LOC Budget

```
xstory/
├── Cargo.toml                  ─   20 LOC
├── src/
│   ├── main.rs                 ─  120 LOC   CLI dispatch, panic hook, signal setup
│   ├── error.rs                ─   50 LOC   enum Error, Display, From impls
│   ├── record.rs               ─  150 LOC   Ingestion pipeline (filter → serialize → write)
│   ├── arena.rs                ─  450 LOC   mmap, parse, ByteView, columnar Arena, dedup, config
│   ├── search.rs               ─  350 LOC   Bitmask, nucleo matcher, Top-K heap, parallel dispatch
│   ├── tui.rs                  ─  550 LOC   State machine, event loop, layout, viewport, render
│   └── syntax.rs               ─  180 LOC   tree-sitter highlight, token cache, ANSI spans
│                               ────────
│                          Total: ~1870 LOC
```

**Target: < 2000 LOC.** Every module is self-contained. No `mod.rs`, no re-exports, no barrel files.

---

## 2. Core Data Types (Complete Definitions)

### 2.1. `error.rs` — единственный тип ошибки крейта

```rust
use std::fmt;

pub enum Error {
    Io(std::io::Error),
    TerminalQuery,
    InvalidArgs(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e)          => write!(f, "io: {e}"),
            Error::TerminalQuery  => f.write_str("terminal query failed"),
            Error::InvalidArgs(m) => write!(f, "invalid args: {m}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e) }
}

// exit code mapping
impl Error {
    pub fn exit_code(&self) -> i32 { 1 }
}
```

### 2.2. `arena.rs` — ядро данных

```rust
use memmap2::Mmap;

/// Cache-aligned string descriptor. 16 bytes, fits 4 per L1 cache line.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct ByteView {
    pub len:    u32,
    pub prefix: [u8; 4],
    pub data:   [u8; 8],  // inlined tail OR (buffer_id: u32, offset: u32)
}

/// Candidate scored for Top-K heap. 8 bytes, Copy.
/// Custom Ord: REVERSE score order so std::collections::BinaryHeap (max-heap)
/// keeps the MINIMUM score at peek(). Ties broken by minimum idx.
/// This gives O(1) rejection of weak candidates when heap is full.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Scored {
    pub score: u32,
    pub idx:   u32,
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse score: lower score = "greater" in heap ordering → floats to top
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Local,
}

/// Terminal color scheme.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

/// Application configuration (env vars + optional ~/.config/xstory/config, no serde).
/// Loaded once at startup via `Config::load()`.
/// File format: plain `key = value` lines, `#` comments, blank lines ignored.
/// Env vars override file values. Compiled-in defaults as final fallback.
pub struct Config {
    pub history_path:     std::path::PathBuf,
    pub scroll_off:       u16,
    pub exclude_commands: Vec<String>,  // default: ["exit","logout","clear"]
    pub secret_patterns:  Vec<String>,  // default: ["AWS_SECRET","GITHUB_TOKEN","--password"]
}

/// Columnar in-memory arena. Owns the mmap and all decoded columns.
/// Lifetime-free: indices and ByteView point into `mmap` or `unescape_buf`.
///
/// If the history file is missing or empty, `mmap` is `None` and all
/// column vectors are empty — this is a valid initial state, not an error.
pub struct Arena {
    // --- owned backing buffers ---
    mmap:          Option<Mmap>,           // buffer 0 (None if file missing/empty)
    unescape_buf:  Vec<u8>,              // buffer 1 (unescaped strings > 12 bytes with \\ sequences)

    // --- parallel column vectors (raw, including duplicates) ---
    pub timestamps:  Vec<i64>,
    pub durations:   Vec<u32>,
    pub exit_codes:  Vec<i32>,
    pub commands:    Vec<ByteView>,
    pub cwds:        Vec<ByteView>,
    pub masks:       Vec<u64>,

    // --- deduplicated scope indices (precomputed in load) ---
    // Built by reverse-chronological scan with FNV-1a HashSet:
    // only the most recent occurrence of each unique command is kept.
    pub global_indices: Vec<u32>,         // unique command indices, newest-first
    pub local_indices:  Vec<u32>,         // same, filtered to cwd == $PWD
}
```

**ByteView resolution (index-based, zero UB):**

```rust
impl Arena {
    /// Resolve a command ByteView by column index to the actual byte slice.
    /// Zero-copy: returns a reference into self.commands[idx] (inlined),
    /// self.mmap (buffer 0), or self.unescape_buf (buffer 1).
    ///
    /// Takes idx (not ByteView by value) so that inlined data is read from
    /// the Vec-backed memory that outlives the call — no dangling stack refs.
    pub fn cmd_bytes(&self, idx: u32) -> &[u8] {
        self.resolve_bv(&self.commands[idx as usize])
    }

    pub fn cwd_bytes(&self, idx: u32) -> &[u8] {
        self.resolve_bv(&self.cwds[idx as usize])
    }

    /// Core resolver: takes &ByteView (borrowed from self.commands/cwds Vec),
    /// so inlined bytes live as long as &self.
    fn resolve_bv(&self, bv: &ByteView) -> &[u8] {
        let len = bv.len as usize;
        if len <= 12 {
            // Inlined: prefix[0..4] ++ data[0..min(len-4, 8)]
            // Safety: bv is borrowed from self.commands/cwds, which lives as long as &self.
            // The prefix and data fields are contiguous in repr(C) layout.
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

    /// Resolve to &str. Unchecked: TSV parse already validated UTF-8.
    #[inline]
    pub fn cmd_str(&self, idx: u32) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.cmd_bytes(idx)) }
    }

    #[inline]
    pub fn cwd_str(&self, idx: u32) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.cwd_bytes(idx)) }
    }
}
```

**Parse pipeline (the only validation boundary):**

```rust
impl Arena {
    /// Load history from file path. Single allocation boundary.
    /// Returns Arena with all columns populated, corrupted lines silently skipped.
    /// If the file is missing or empty, returns an empty Arena (not an error).
    pub fn load(path: &std::path::Path, local_cwd: Option<&str>) -> Result<Arena, crate::Error>;
}
```

Inside `load`:
1. Try to open and `mmap` the file read-only. If missing or size == 0 → return empty `Arena` (all vecs empty, `mmap: None`).
2. Scan for `\n` boundaries (memchr).
3. For each line: split on `\t`, expect exactly 5 fields. Malformed → skip silently.
4. Parse `timestamp_ms`, `duration_ms`, `exit_code` via inline `atoi` (no alloc).
5. Build `ByteView` for `cwd` and `command`:
   - If no escapes and `len ≤ 12` → inline into ByteView.
   - If no escapes and `len > 12` → out-of-line pointing into `mmap` (buffer 0).
   - If has escapes → unescape into `unescape_buf`, record offset, out-of-line (buffer 1).
6. Compute 64-bit char mask for command.
7. **Deduplication pass (reverse-chronological):**
   - Walk raw entries from last to first.
   - Maintain `HashSet<u64>` keyed by FNV-1a hash of command bytes.
   - On collision: compare full byte slices to handle hash collisions.
   - First (newest) occurrence of each unique command → push to `global_indices`.
   - If `local_cwd.is_some()` and cwd matches → also push to `local_indices`.
   - After scan, reverse both index vectors to restore newest-first order.

**Result:** `search.rs` operates exclusively on `arena.scope_indices(scope)` — already deduplicated, no per-keystroke `HashSet`, no rayon chunk coordination issues.

### 2.3. `record.rs` — types

```rust
/// Policy verdict for a command string.
enum Verdict {
    Allow,
    Deny,
    Mask, // placeholder replacement applied
}

/// Pre-validated record ready for serialization. 
/// Constructed only via `Record::parse()` — invalid states are impossible.
pub struct Record<'a> {
    timestamp_ms: i64,
    duration_ms:  u64,
    exit_code:    i32,
    cwd:          &'a str,
    command:      &'a str, // possibly masked
}
```

### 2.4. `search.rs` — types

```rust
/// Parsed query tokens extracted once per keystroke.
#[derive(Clone, Copy)]
pub enum QueryMode<'a> {
    Empty,
    Prefix(&'a str),       // ^foo
    Substring(&'a str),    // 'foo
    Suffix(&'a str),       // foo$
    Fuzzy(&'a str),        // default
}

/// Immutable search result: indices into Arena, sorted by score descending.
pub struct SearchResult {
    pub indices: Vec<u32>,   // up to K=1000 indices into Arena columns
    pub total:   u32,        // total entries that passed scope filter
}
```

### 2.5. `tui.rs` — state

```rust
/// The entire TUI state. Owned by `run_tui`, mutated in-place.
pub struct Dashboard {
    // --- input ---
    query:           String,      // owned, mutated by user
    cursor:          usize,       // char position (not byte)

    // --- search ---
    scope:           Scope,
    result:          SearchResult,
    dedup_total:     usize,       // arena.scope_indices(scope).len() — total unique entries

    // --- navigation ---
    selected:        usize,       // index into result.indices
    viewport_offset: usize,       // first visible row
    preview_scroll:  usize,

    // --- display ---
    theme:           Theme,
    width:           u16,
    height:          u16,

    // --- frame buffer ---
    dirty:           bool,
}

/// Terminal exit outcome — determines process exit code.
pub enum Outcome {
    Execute(String),  // exit 0
    Edit(String),     // exit 2
    Cancel,           // exit 130
}
```

### 2.6. `syntax.rs` — types

```rust
/// A colored span within a command string.
#[derive(Clone, Copy)]
pub struct Span {
    pub start: u16,
    pub end:   u16,
    pub kind:  TokenKind,
}

/// Bash token categories for coloring.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
    slots: Box<[(u64, Vec<Span>); 2048]>,  // (fnv1a_hash, spans)
}

/// Preview cache (128 slots) for syntax-highlighted multi-line blocks.
pub struct PreviewCache {
    slots: Box<[(u64, Vec<Span>); 128]>,
}
```

---

## 3. Module Boundary Signatures (Ownership Flow)

### `main.rs` → everything

```rust
fn main() {
    // 1. Parse CLI args (hand-rolled, no clap)
    // 2. Dispatch:
    match cli_action {
        Action::Record { cmd, exit, dur, cwd } => {
            record::record(&cmd, exit, dur, cwd.as_deref())?;
            // record borrows args, writes to disk, returns ()
        }
        Action::Search { initial_query } => {
            install_panic_hook();     // before raw mode
            install_signal_handlers();
            let arena = arena::Arena::load(&config.history_path, Some(&cwd))?;
            // Arena OWNS the mmap and all columns
            let outcome = tui::run_tui(&arena, &config, initial_query.as_deref())?;
            // run_tui BORROWS arena immutably for its lifetime
            match outcome {
                Outcome::Execute(cmd) => { print!("{cmd}"); std::process::exit(0); }
                Outcome::Edit(cmd)    => { print!("{cmd}"); std::process::exit(2); }
                Outcome::Cancel       => { std::process::exit(130); }
            }
        }
        Action::Init { shell } => {
            // Print shell hook script to stdout
            print_shell_hook(shell);
        }
    }
}
```

### `record.rs`

```rust
/// Hot path: filter → serialize → atomic append. ≤1 allocation.
/// Borrows all inputs. Returns Ok(()) or Err on I/O failure.
pub fn record(
    cmd_parts: &[String],
    exit_code: i32,
    duration_ms: u64,
    cwd: Option<&str>,
) -> Result<(), crate::Error>;
```

Inside:
1. `join(cmd_parts, " ")` → check empty/whitespace/ignorespace → check exclude list → check secrets (prefix match, not regex — see note below).
2. Serialize to stack `Vec<u8>` (capacity: 64 + cmd.len() + cwd.len()).
3. `OpenOptions::new().append(true).create(true).mode(0o600).open(path)?.write_all(&buf)?`

> **Secrets policy:** Instead of heavy regex, use prefix substring matching against a compiled-in list of known patterns (`AWS_SECRET`, `GITHUB_TOKEN`, `--password`, etc.). O(1) amortized, stays within 0.1ms budget.

### `arena.rs`

```rust
/// Parse mmap'd TSV into columnar Arena. Single allocation boundary.
/// Missing/empty file → empty Arena (not an error).
pub fn load(path: &Path, local_cwd: Option<&str>) -> Result<Arena, Error>;

/// Resolve command at index → &[u8] (zero-copy from Vec, mmap, or unescape_buf).
pub fn cmd_bytes(&self, idx: u32) -> &[u8];

/// Resolve command at index → &str (unchecked UTF-8, validated at parse time).
pub fn cmd_str(&self, idx: u32) -> &str;

/// Resolve cwd at index → &str.
pub fn cwd_str(&self, idx: u32) -> &str;

/// Compute 64-bit character bitmask for a query string.
pub fn char_mask(s: &[u8]) -> u64;

/// Get the deduplicated indices slice for the active scope.
/// Returns &self.global_indices or &self.local_indices.
pub fn scope_indices(&self, scope: Scope) -> &[u32];

/// Load config from env vars + ~/.config/xstory/config (30-line key=value parser).
pub fn load_config() -> Config;
```

### `search.rs`

```rust
/// Execute search pipeline: bitmask prune → nucleo fuzzy → Top-K heap.
/// Borrows Arena immutably. Returns owned SearchResult (Vec<u32> of indices).
pub fn search(
    arena: &Arena,
    query: &str,
    scope: Scope,
) -> SearchResult;
```

Inside:
1. Parse query → `QueryMode`.
2. If `Empty` → return first K indices in reverse chrono.
3. Compute `query_mask = char_mask(query)`.
4. If count < 500 → single-thread. Else → rayon par_chunks(32768).
5. Per chunk: bitmask prune → nucleo match → local BinaryHeap<Scored> (K=1000).
6. Merge local heaps → final sorted `Vec<u32>`.

### `tui.rs`

```rust
/// Run the interactive TUI. Borrows Arena for the entire session.
/// Returns Outcome determining the process exit code.
pub fn run_tui(
    arena: &Arena,
    config: &Config,
    initial_query: Option<&str>,
) -> Result<Outcome, Error>;
```

Inside — the entire event loop on one page:

```rust
pub fn run_tui(arena: &Arena, config: &Config, initial_q: Option<&str>) -> Result<Outcome, Error> {
    let theme = detect_theme();
    let (w, h) = crossterm::terminal::size()?;
    let mut dash = Dashboard::new(theme, w, h, initial_q);
    dash.result = search::search(arena, &dash.query, dash.scope);

    let mut terminal = setup_terminal()?;  // raw mode, alternate screen, stderr

    loop {
        if dash.dirty {
            terminal.draw(|f| render_frame(f, &dash, arena))?;
            dash.dirty = false;
        }

        match crossterm::event::read()? {
            // --- query mutation ---
            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => {
                dash.insert_char(c);
                dash.result = search::search(arena, &dash.query, dash.scope);
                dash.selected = 0;
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => {
                dash.delete_char_before();
                dash.result = search::search(arena, &dash.query, dash.scope);
                dash.selected = 0;
                dash.dirty = true;
            }

            // --- navigation ---
            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                dash.move_selection(-1);
                dash.dirty = true;
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                dash.move_selection(1);
                dash.dirty = true;
            }

            // --- scope toggle ---
            Event::Key(KeyEvent { code: KeyCode::Char('g'), modifiers: KeyModifiers::CONTROL, .. }) => {
                dash.toggle_scope();
                dash.result = search::search(arena, &dash.query, dash.scope);
                dash.dirty = true;
            }

            // --- terminal actions ---
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                restore_terminal(&mut terminal)?;
                let cmd = dash.selected_command(arena);
                return Ok(Outcome::Execute(cmd));
            }
            Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => {
                restore_terminal(&mut terminal)?;
                let cmd = dash.selected_command(arena);
                return Ok(Outcome::Edit(cmd));
            }
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                restore_terminal(&mut terminal)?;
                return Ok(Outcome::Cancel);
            }

            Event::Resize(w, h) => {
                dash.resize(w, h);
                dash.dirty = true;
            }
            _ => {}
        }
    }
}
```

### `syntax.rs`

```rust
/// Tokenize a command string into colored spans using tree-sitter.
/// Uses the direct-mapped cache to avoid re-parsing unchanged strings.
pub fn tokenize(cache: &mut TokenCache, cmd: &str) -> &[Span];

/// Produce full syntax-highlighted preview for a multi-line command.
pub fn preview(cache: &mut PreviewCache, cmd: &str) -> &[Span];

/// Map TokenKind to ANSI color based on Theme.
pub fn style_for(kind: TokenKind, theme: Theme) -> ratatui::style::Style;
```

---

## 4. Ownership & Lifetime Map

```
main()
  │
  ├── Arena::load()        → Arena { mmap?, unescape_buf, columns, global_indices, local_indices }
  │                           ↑ OWNS all data. Empty file → empty Arena (not error).
  │                           Deduplication done here: newest-wins via reverse HashSet scan.
  │
  └── run_tui(&arena, ...)
        │
        ├── Dashboard       → OWNS query: String, result: SearchResult
        │                     Indexes into arena via u32 indices
        │
        ├── search(&arena)  → SearchResult { indices: Vec<u32> }
        │     ↑ borrows arena immutably
        │     Operates on arena.scope_indices() — already deduplicated
        │     Returns owned Vec of indices (not references)
        │
        ├── TokenCache      → OWNS cached Span vecs (keyed by hash)
        │     tokenize() borrows cmd: &str
        │
        └── render_frame(&Dashboard, &Arena)
              ↑ borrows both immutably
              Resolves arena.cmd_str(idx) → &str (zero-copy)
```

**Key design decisions:**
- No lifetimes propagate between modules. `Arena` owns the buffers. Everything else uses `u32` indices or borrows `&Arena` for the call.
- `SearchResult` carries `Vec<u32>` — owned, movable, no lifetime parameter. Zero explicit lifetime annotations outside of `Arena::resolve_bv`.
- Deduplication is a **load-time concern**, not a search-time concern. `global_indices` and `local_indices` are pre-computed once. `search.rs` never allocates a `HashSet`.
- Missing/empty history file is a valid initial state (`Arena` with empty vecs), not `Error::MmapEmpty`.

---

## 5. Dependency Manifest

```toml
[package]
name = "xstory"
version = "0.1.0"
edition = "2024"

[dependencies]
crossterm = "0.28"
ratatui = { version = "0.29", default-features = false, features = ["crossterm"] }
memmap2 = "0.9"
nucleo-matcher = "0.3"
tree-sitter = "0.24"
tree-sitter-bash = "0.23"
rayon = "1.10"
memchr = "2.7"

[profile.release]
lto = true
codegen-units = 1
strip = true
panic = "abort"

[lints.clippy]
pedantic = { level = "warn", priority = -1 }
nursery = { level = "warn", priority = -1 }
unwrap_used = "warn"
```

**Zero gratuitous deps:** No `thiserror`, no `chrono`, no `clap`, no `serde`, no `toml`, no `aho-corasick`, no `anyhow`.

---

## 6. Build Plan (Sequential, Each Step Compiles & Tests)

### Step 1: `error.rs` + `Cargo.toml` skeleton
**Deliverable:** `cargo check` passes. Flat `Error` enum with `Display` + `From<io::Error>`.
**Test:** Unit test formatting each variant.

### Step 2: `arena.rs` — ByteView + Parse + Dedup + Config
**Deliverable:** `Arena::load()` parses a sample TSV file. ByteView inline/out-of-line encoding. Dedup via reverse HashSet. `Config::load()` reads env + key=value file.
**Test:**
- Round-trip: write TSV → load → verify all columns.
- Corrupted lines silently skipped.
- Empty/missing file → empty Arena (all vecs empty, no error).
- Escaping: `\t`, `\n`, `\\` in command strings.
- ByteView inline (≤12 bytes) vs out-of-line (>12 bytes).
- `char_mask` computation + fast-reject property.
- Dedup: 3 entries with same command → `global_indices` contains only the newest.
- Dedup: hash collision (different commands, same FNV hash) → both preserved.
- Scope: `local_indices` contains only entries matching `local_cwd`.
- Config: env var `XSTORY_HISTORY_PATH` overrides file and default.

### Step 3: `record.rs` — Ingestion Pipeline
**Deliverable:** `record()` writes a valid TSV line with atomic append.
**Test:**
- Empty/whitespace commands rejected.
- Ignorespace (leading space) rejected.
- Excluded commands (`exit`, `logout`) rejected.
- Secret patterns masked.
- Special chars properly escaped.
- File permissions enforced (0o600).
- Round-trip: `record()` → `Arena::load()` → verify fields.

### Step 4: `search.rs` — Bitmask + Nucleo + Top-K
**Deliverable:** `search()` returns correct Top-K results.
**Test:**
- Empty query → reverse chronological order.
- Prefix `^foo` → only commands starting with "foo".
- Substring `'foo` → only commands containing "foo".
- Suffix `foo$` → only commands ending with "foo".
- Fuzzy ranking: exact match scores higher than partial.
- Bitmask pruning correctness (no false negatives).
- Top-K heap: exactly K results, sorted by score desc.
- Parallel vs sequential produces identical results.
- Scope filtering (Global vs Local).

### Step 5: `syntax.rs` — Tree-sitter Tokenization
**Deliverable:** `tokenize()` and `preview()` return colored spans.
**Test:**
- Simple command → `[Command, Plain]`.
- Pipeline `ls | grep foo` → correct operator spans.
- Cache hit: second call same string → no re-parse.
- Cache eviction: different hash, same slot → replaced.

### Step 6: `tui.rs` — State Machine + Render
**Deliverable:** Full interactive TUI over `stderr`.
**Test:** (integration/manual)
- Theme detection cascade (OSC 11 → env → OS → default dark).
- Viewport scrolloff behavior.
- Resize handling (including degenerate sizes < 5 rows).
- All key bindings per state transition table.
- Exit codes: Enter→0, Tab→2, Esc→130.

### Step 7: `main.rs` — CLI Dispatch + Signals
**Deliverable:** Complete binary. Hand-rolled arg parser.
**Test:**
- `xstory record -- "ls -la" 0 100` → appends to TSV.
- `xstory search` → launches TUI.
- `xstory init bash` → prints shell hook.
- `xstory init zsh` → prints shell hook.
- `xstory init fish` → prints shell hook.
- Unknown subcommand → exit 1 with message.
- Panic hook restores terminal.
- SIGINT/SIGTERM restores terminal.

---

## 7. CLI Interface (Hand-Rolled)

```
xstory record <command...> --exit=<i32> --duration=<u64> [--cwd=<path>]
xstory search [--query=<str>]
xstory init <bash|zsh|fish>
xstory --help
xstory --version
```

Parser: ~40 lines of `std::env::args()` + manual matching. No clap, no structopt.

---

## 8. Invariants Checklist

- [ ] `stdout` is NEVER written to during TUI — all rendering goes to `stderr`
- [ ] `O_APPEND` used for all history writes — no file locks
- [ ] `Arena::resolve_bv(&ByteView)` borrows from `self.commands`/`self.cwds` Vec — no dangling stack refs
- [ ] `Arena::load()` returns empty Arena for missing/empty file — no `MmapEmpty` error
- [ ] Deduplication happens once in `Arena::load()`, not per-keystroke in search
- [ ] `Scored` has reverse `Ord` so `BinaryHeap::peek()` returns minimum score for O(1) rejection
- [ ] Panic hook registered BEFORE entering raw mode
- [ ] Signal handlers (SIGINT, SIGTERM) restore terminal before re-raising
- [ ] No `.clone()` in hot paths (search, render)
- [ ] No `trait` definitions anywhere in the crate
- [ ] No types with suffix Manager/Service/Handler/Coordinator/Context
- [ ] All explicit lifetimes confined to `Arena::resolve_bv` return values
- [ ] `crossterm` has no `event-stream` feature (synchronous `event::read()` only)
- [ ] Shell hooks: `bash`, `zsh`, `fish` — all three supported
- [ ] Config: env vars → `~/.config/xstory/config` (key=value) → compiled-in defaults
- [ ] `cargo clippy --all-targets -- -D warnings` passes with pedantic+nursery
