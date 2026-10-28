<p align="center">
  <a href="https://github.com/zx0r/xstory/releases">
    <img src="https://img.shields.io/github/v/release/zx0r/xstory?color=4be1ff&label=RELEASE&style=flat-square&logo=github&labelColor=1a1a1a" alt="Release" />
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/LICENSE-MIT%20%2F%20Apache--2.0-9fef00?style=flat-square&logo=opensourceinitiative&logoColor=white&labelColor=1a1a1a" alt="License" />
  </a>
  <a href="https://www.rust-lang.org/">
    <img src="https://img.shields.io/badge/CORE-RUST-orange?style=flat-square&logo=rust&logoColor=white&labelColor=1a1a1a" alt="Rust" />
  </a>
  <a href="https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html">
    <img src="https://img.shields.io/badge/XDG-COMPLIANT-00FF00?style=flat-square&logo=linux&logoColor=white&labelColor=1a1a1a" alt="XDG Compliant" />
  </a>
</p>

<h1 align="center">xstory ⚡</h1>

<p align="center">
  <strong>Minimalist Unix shell history engine and interactive fuzzy picker for Fish, Zsh, and Bash.</strong><br />
  <em>Zero daemons. Zero SQLite lock contention. Columnar memory-mapped arena. «Scan → Inspect» ergonomics.</em>
</p>

---

## Why xstory?

Modern shell history tools often introduce daemon processes, background sockets, and embedded relational databases (SQLite) that cause lock contention (`SQLITE_BUSY`), shell prompt lag, and multi-megabyte binary bloat.

`xstory` returns to the **Unix philosophy**:
1. **Append-Only POSIX Journal:** Writes history directly to disk via atomic POSIX `O_APPEND`. No daemons, no background threads, no lock contention.
2. **Columnar In-Memory Arena:** Memory-mapped (`memmap2`) storage with 16-byte `ByteView` descriptors (inlined ≤12B, out-of-line >12B) and a 64-bit character presence bitmask for single-cycle candidate rejection.
3. **Sub-Millisecond Search:** Powered by `nucleo-matcher` with bounded min-heaps ($O(N \log K)$) across 100,000+ entries.
4. **«Scan → Inspect» Visual Model:** Clean, unclipped left-to-right command scanning in the history panel, accompanied by instant in-process Tree-sitter Bash AST syntax highlighting in the preview panel.
5. **Tiny Footprint:** Clean-room implementation under 2,000 LOC, compiling to a self-contained ~2.7 MB binary with zero runtime dependencies.

---

## Architectural Comparison: xstory vs. Atuin

| Dimension / Metric | Atuin (v18.x) | xstory | Architectural Advantage |
|:---|:---:|:---:|:---|
| **Storage Architecture** | SQLite DB (WAL) + Background Daemon | POSIX Append-Only TSV Journal | **Zero daemon overhead**, no socket stalls |
| **Concurrency Model** | SQLite write-lock serialization (`SQLITE_BUSY`) | Atomic POSIX `O_APPEND` (kernel inode lock) | **100% lock-free** multi-terminal concurrent writes |
| **Binary Size** | ~58.9 MB | **~2.7 MB** | **21.8× smaller** self-contained executable |
| **Cold Startup Latency** | ~7.5 ms | **~4.6 ms** | **1.6× faster** execution |
| **Shell Prompt Lag** | ~15.3 ms (two-phase preexec/postexec IPC) | **~4.7 ms** (single-phase native hook) | **3.2× faster** prompt return |
| **Disk Append Latency** | Multi-millisecond SQLite transaction | **< 100 µs** direct POSIX append | Zero perceptible typing lag |
| **Search Engine** | SQL queries + in-memory filtering | `nucleo-matcher` + 64-bit bitmask + Top-K min-heap | Sub-millisecond matching on 100k+ entries |
| **Syntax Highlighting** | Regex / external syntax engine | In-process **Tree-sitter Bash AST** (< 15 µs) | Real syntax tokenization with zero process forks |
| **Crate Dependencies** | 700+ crates | < 200 crates | Minimal attack surface and rapid compilation |

---

## TUI Ergonomics: «Scan → Inspect»

Conventional history search tools cram timestamps, durations, exit statuses, and paths into every row, leading to severe visual clutter and arbitrary middle-ellipsizing (`docker run -d…--restart always`).

`xstory` enforces a strict division of responsibility:

```text
┌─[ GLOBAL: 100,000 ENTRIES ]────────────────────────────────── [Esc] Exit • [^R] Scope ─┐
│ > cargo test --workspace --all-targets                                             ~2m │
│   git commit -m "fix(tui): clean query cursor jump"                                ~5m │
│   docker compose up -d postgres redis                                             ~12m │
│   ./scripts/deploy.sh --production                                                 ~1h │
├─[ 29 Oct 00:15 • 450ms • exit 0 • ~/dev/own/xstory ]───────────────────────────────────┤
│ cargo test \                                                                           │
│   --workspace \                                                                        │
│   --all-targets \                                                                      │
│   -- --nocapture                                                                       │
└────────────────────────────────────────────────────────────────────────────────────────┘
┌─[                                                                                   ]─┐
```

* **History Panel (Fast Scan Area):**
  * Commands render in pure monospace without truncation artifacts or middle-ellipsizing.
  * Elapsed time (`~2m`, `~1h`) is pinned to the right edge.
  * Failed commands (`exit != 0`) are signaled by a subtle crimson timestamp accent without layout jitter.
  * Reverse-chronological: latest commands sit at the bottom, directly above the input bar.
* **Preview Panel (Deep Inspection Area):**
  * Absolute timestamp, duration, exit code, and working directory are consolidated cleanly in the border header.
  * Multi-line commands and escaped continuations are fully preserved and syntax-highlighted via Tree-sitter.
  * Supports content-adaptive `Dynamic` height (default) or `Fixed` height via `--preview=fixed`.
* **Search Input (Zero-Jitter):**
  * Discreet magnifying glass icon (``) disappears seamlessly upon typing.
  * Typed command begins at column 0 with zero cursor jump.

---

## Installation

### Homebrew (macOS / Linux)
```bash
brew install zx0r/tap/xstory
```

### Cargo
```bash
cargo install xstory
```

### Build from Source
```bash
git clone https://github.com/zx0r/xstory.git
cd xstory
cargo build --release
cp ./target/release/xstory ~/.local/bin/
```

---

## Shell Integration

`xstory` integrates with native shell line editors with zero external binary forks during prompt rendering.

### Fish (`~/.config/fish/config.fish`)
```fish
xstory init fish | source
```

### Zsh (`~/.zshrc`)
```zsh
eval "$(xstory init zsh)"
```

### Bash (`~/.bashrc`)
```bash
eval "$(xstory init bash)"
```

---

## Migration & History Import

Import legacy history from your existing shell files with automatic deduplication:

```bash
# Auto-detect and import history from Fish, Zsh, or Bash
xstory history import

# Import from a specific file
xstory history import ~/.zsh_history

# Export history back to TSV
xstory history export backup.tsv
```

---

## Keybindings

| Key | Action |
|:---|:---|
| <kbd>↑</kbd> or <kbd>Ctrl</kbd>+<kbd>R</kbd> | Open interactive history search from shell prompt |
| <kbd>↑</kbd> / <kbd>↓</kbd> | Navigate candidates (bottom-up: newest to oldest) |
| <kbd>PgUp</kbd> / <kbd>PgDn</kbd> | Jump 10 entries up / down |
| <kbd>Alt</kbd>+<kbd>↑</kbd> / <kbd>Alt</kbd>+<kbd>↓</kbd> | Scroll preview panel content |
| <kbd>Ctrl</kbd>+<kbd>R</kbd> / <kbd>Ctrl</kbd>+<kbd>G</kbd> | Toggle Scope: **Global** (all history) $\leftrightarrow$ **Local** (current `$PWD`) |
| <kbd>Enter</kbd> | Execute selected command immediately |
| <kbd>Tab</kbd> | Put selected command into commandline buffer for editing |
| <kbd>Ctrl</kbd>+<kbd>U</kbd> / <kbd>Ctrl</kbd>+<kbd>W</kbd> | Clear query to start / delete word before cursor |
| <kbd>Esc</kbd> / <kbd>Ctrl</kbd>+<kbd>C</kbd> | Cancel search and return to shell prompt |

---

## Configuration

`xstory` adheres to the **XDG Base Directory** specification:
* **History Journal:** `~/.local/share/xstory/history.tsv`
* **Configuration:** `~/.config/xstory/config`

```ini
# Preview panel mode: 'dynamic' (adaptive height) or 'fixed'
preview = dynamic

# Number of rows to keep above/below selection when scrolling
scroll_off = 4

# Commands excluded from recording
exclude_commands = exit, logout, clear

# Substrings to redact before writing to disk
secret_patterns = AWS_SECRET, GITHUB_TOKEN, --password
```

### CLI Flags & Options
```bash
xstory [OPTIONS] [QUERY] [COMMAND]

Commands:
  history   Import or export history logs
  record    Record command execution into history log
  init      Initialize shell integration hook (fish | zsh | bash)

Options:
  -H, --history <PATH>   Path to history file
  -s, --scope <SCOPE>    Search scope (global / local)
  -p, --preview <MODE>   Preview mode (dynamic / fixed)
  -h, --help             Print help
  -V, --version          Print version
```

### Environment Variables
* `XSTORY_HISTORY_PATH`: Override history log file location.
* `XSTORY_PREVIEW`: Override preview mode (`dynamic` or `fixed`).
* `XSTORY_SCROLL_OFF`: Override scroll cushion.
* `XSTORY_DISABLE`: Temporarily disable history recording in current session.

---

## Security & Privacy

* **Process Table Safety:** Commands are ingested via `stdin` by the shell hook, preventing sensitive commands from ever appearing in `ps aux` or `/proc`.
* **Automatic Secret Masking:** Configured patterns (e.g. `--password`, `AWS_SECRET`, tokens) are redacted before disk write.
* **Ignorespace Compliance:** Any command executed with a leading space is never written to history.
* **Atomic Non-Interleaved Writes:** Direct POSIX `O_APPEND` syscalls ensure multi-terminal concurrent writes up to `PIPE_BUF` (4,096 bytes) never corrupt lines.

---

## License

Dual-licensed under [MIT](LICENSE) or [Apache-2.0](LICENSE-APACHE).
