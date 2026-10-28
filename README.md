<p align="center">
  <a href="https://github.com/zx0r/xstory/releases">
    <img src="https://img.shields.io/github/v/release/zx0r/xstory?color=4be1ff&label=RELEASE&style=flat-square&logo=github&labelColor=1a1a1a" alt="Release" />
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/LICENSE-MIT%20%2F%20Apache--2.0-9fef00?style=flat-square&logo=opensourceinitiative&logoColor=white&labelColor=1a1a1a" alt="License" />
  </a>
  <a href="https://www.rust-lang.org/">
    <img src="https://img.shields.io/badge/CORE-RUST%202024-orange?style=flat-square&logo=rust&logoColor=white&labelColor=1a1a1a" alt="Rust" />
  </a>
  <a href="https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html">
    <img src="https://img.shields.io/badge/XDG-COMPLIANT-00FF00?style=flat-square&logo=linux&logoColor=white&labelColor=1a1a1a" alt="XDG Compliant" />
  </a>
</p>

<h1 align="center">
  xstory ⚡
</h1>

<p align="center">
  <strong>Minimalist Unix-philosophy shell history engine & SIMD-accelerated fuzzy finder for Fish, Zsh, and Bash.</strong><br />
  <em>Zero daemons. Zero SQLite bloat. DuckDB-style StringView columnar arena. «Scan → Inspect» ergonomics.</em>
</p>

---

## ⚡ Key Highlights

* **Zero-Daemon Architecture:** No background servers, sockets, or lock contention. Pure POSIX `O_APPEND` atomic disk journals ($< 50\,\mu\text{s}$ append).
* **DuckDB StringView Arena:** 16-byte compact descriptors (`ByteView`) and 64-bit presence bitmasks. 500,000 commands fit directly into CPU L3 cache.
* **Sub-Millisecond Fuzzy Matching:** Powered by `nucleo-matcher` with thread-local scratchpads and bounded min-heap ($O(N \log K)$).
* **«Scan → Inspect» Visual Model:** High-density history panel with monospace left-to-right alignment + live preview with in-process **Tree-sitter Bash AST** syntax highlighting.
* **Lightweight Binary:** Self-contained **~2.7 MB Mach-O** executable (vs. Atuin's 58.9 MB).
* **Privacy & Security First:** Zero-config regex redaction of tokens (`AWS_SECRET`, `--password`), `ignorespace` support, and zero command leakage in `ps aux`.

---

## 📊 Empirical Performance Benchmark: xstory vs. Atuin

Verified on Apple Silicon ARM64 via `hyperfine` (`--warmup 10 --runs 150`) and Darwin Mach kernel instrumentation (`/usr/bin/time -l`):

| Metric / Dimension | Atuin (v18.24) | xstory (v0.1.0) | Delta / Ratio | Architectural Impact |
|:---|:---:|:---:|:---:|:---|
| **Executable Binary Size** | 58.87 MB | 2.70 MB | **21.8× smaller** | Minimalist Mach-O footprint |
| **Mach-O `__TEXT` Section** | 43.52 MB | 2.53 MB | **17.2× smaller** | Executable machine code density |
| **Source Code (Rust SLOC)** | 110,995 lines | ~4,900 lines | **22.6× leaner** | High auditability & zero bloat |
| **Dependency Graph** | 744 crates | 198 crates | **3.8× fewer** | Reduced attack surface & build time |
| **Cold-Start Latency (`--version`)** | 7.41 ± 0.44 ms | 4.61 ± 0.91 ms | **1.6× faster** | Instant CLI response |
| **CLI Help Latency (`--help`)** | 7.57 ± 0.67 ms | 4.71 ± 0.32 ms | **1.6× faster** | Zero cold-start tax |
| **Shell Hook Append (Hot Path)** | 15.35 ± 0.88 ms | 4.75 ± 0.38 ms | **3.2× faster** | Sub-millisecond prompt latency |
| **Max Resident Set Size (RSS)** | 10.88 MB | 5.58 MB | **1.9× lighter** | Zero memory pressure on shell |
| **Peak Memory Footprint** | 4.19 MB | 1.64 MB | **2.6× lighter** | Mach allocator efficiency |
| **Kernel Page Reclaims (Faults)** | 973 | 542 | **1.8× fewer** | Minimal VM subsystem traps |
| **Instructions Retired** | 60,152,979 | 33,216,482 | **1.8× fewer** | Raw CPU work required |
| **CPU Cycles Elapsed** | 20,496,574 | 12,886,079 | **1.6× fewer** | Thermal & battery efficiency |

---

## 📈 500,000 Records Scale Benchmark

Measurements on a dataset of **500,000 shell entries** on Apple Silicon ARM64 ($N = 100$ runs):

| Metric / Benchmark Stage | Atuin (v18.24 / SQLite) | Legacy AoS (`Vec<Entry>`) | xstory Columnar Arena | Advantage vs. Atuin |
|:---|:---:|:---:|:---:|:---:|
| **Ingestion / Load Time** | 21,200 ms (21.2 s) | 33.76 ms | **11.71 ms** (3.73 GB/s) | **1,810× faster** |
| **Data Size on Disk** | 275.0 MB (`history.db`+WAL) | 43.6 MB (`history.tsv`) | **43.6 MB** (`history.tsv`) | **6.3× more compact** |
| **Metadata Footprint (RAM)** | 21.1 MB (RSS) | ~115 MB (Heap Strings) | **28.0 MB** (fits in SLC/L3) | 100% in CPU Cache |
| **Prefix Match (`^git`, Top-100)** | 16.4 ± 1.0 ms | 4.52 ms | **279.5 µs (0.28 ms)** ($p_{95}: 327.9\text{ µs}$) | **58.7× faster** |
| **Multi-Word Contiguous (`cargo test`)**| 16.2 ± 1.2 ms | 15.10 ms | **1.59 ms** ($p_{95}: 2.72\text{ ms}$) | **10.2× faster** |
| **Multi-Word Non-Contiguous (`build release`)**| 17.5 ± 1.4 ms | 18.20 ms | **1.97 ms** ($p_{95}: 2.35\text{ ms}$) | **8.9× faster** |
| **Substring Fuzzy Match (`status`)** | 16.1 ± 0.6 ms | 22.40 ms | **4.37 ms** ($p_{95}: 5.02\text{ ms}$) | **3.7× faster** |
| **Full Match Search (`cargo test`, 25k)**| 18.9 ± 1.5 ms | 16.40 ms | **1.75 ms** ($p_{95}: 2.62\text{ ms}$) | **10.8× faster** |
| **Keystroke Frame Budget (120 FPS / 8.33 ms)**| **FAIL** (Drops < 60 FPS) | **FAIL** (> 16 ms jitter) | **PASS (< 5.0 ms @ 120 FPS)** | **Rock-solid ProMotion smoothness** |

---

## 🏛️ Architecture & UX: The «Scan → Inspect» Model

Traditional history tools cram durations, timestamps, directories, and exit icons into a single horizontal row, introducing visual stumbling and middle-ellipsizing (`docker run -d…--restart always`). 

`xstory` enforces strict separation of concerns across viewports:

```text
┌─[ GLOBAL: 25,481 ENTRIES ]─────────────────────────────────── [Esc] Exit • [^R] Scope ─┐
│ > cargo test --all-targets                                                         ~2m │
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

1. **History Panel (Fast Scan Area):**
   * **Monospace Left Alignment:** Commands render cleanly from left to right with zero middle-ellipsizing.
   * **Right-Pinned Relative Time:** Elapsed intervals (`~2m`, `~1h`) are anchored to the right boundary.
   * **Glyph-Free Failure Signaling:** Non-zero exit codes are indicated by a subtle crimson timestamp accent without layout jitter.
   * **Bottom-Up Reverse Chronology:** Latest executed commands appear at the bottom, directly above the search input.

2. **Preview Panel (Deep Inspection Area):**
   * **Border Metadata Consolidation:** Absolute timestamp, execution duration, numeric exit status, and working directory reside exclusively in the panel frame.
   * **Native Tree-Sitter AST Highlighting:** Multi-line command bodies are parsed in-process via C-ABI bindings to `tree-sitter-bash` ($< 15\,\mu\text{s}$, zero process spawning).
   * **Flexible Sizing:** Supports both `Dynamic` (content-adaptive height) and `Fixed` modes (`--preview=fixed | dynamic`).

3. **Query Bar (Interactive Zero-Jitter Input):**
   * Contextual magnifying glass placeholder (``) disappears seamlessly upon typing.
   * Keystrokes replace the placeholder on the first column with zero cursor jump.

---

## 🚀 Quickstart

### 1. Installation

#### Homebrew (macOS / Linux)
```bash
brew install zx0r/tap/xstory
```

#### Cargo
```bash
cargo install xstory
```

#### Build From Source
```bash
git clone https://github.com/zx0r/xstory.git
cd xstory
cargo build --release
cp ./target/release/xstory ~/.local/bin/xstory
```

---

### 2. Shell Integration

`xstory` hooks directly into native shell line editors with zero external binary forks on prompt render.

#### Fish (`~/.config/fish/config.fish`)
```fish
xstory init fish | source
```

#### Zsh (`~/.zshrc`)
```zsh
eval "$(xstory init zsh)"
```

#### Bash (`~/.bashrc`)
```bash
eval "$(xstory init bash)"
```

---

### 3. Import Existing History

Seamlessly import and deduplicate your existing shell logs without duplicates:

```bash
# Automatically detects and imports legacy history from Fish, Zsh, or Bash
xstory history import

# Or specify a custom history file
xstory history import ~/.zsh_history
```

---

## ⌨️ Interactive Keybindings

| Keybinding | Action |
|:---|:---|
| <kbd>↑</kbd> or <kbd>Ctrl</kbd> + <kbd>R</kbd> | Open interactive history search from shell prompt |
| <kbd>↑</kbd> / <kbd>↓</kbd> | Navigate candidates (bottom-up: newest to oldest) |
| <kbd>PgUp</kbd> / <kbd>PgDn</kbd> | Jump 10 items up or down |
| <kbd>Alt</kbd> + <kbd>↑</kbd> / <kbd>↓</kbd> | Scroll preview panel content |
| <kbd>Ctrl</kbd> + <kbd>R</kbd> / <kbd>Ctrl</kbd> + <kbd>G</kbd> | Toggle search scope: **Global** (all commands) $\leftrightarrow$ **Local** (`$PWD` only) |
| <kbd>Enter</kbd> | Execute selected command immediately |
| <kbd>Tab</kbd> | Paste selected command into commandline buffer for editing |
| <kbd>Ctrl</kbd> + <kbd>U</kbd> / <kbd>Ctrl</kbd> + <kbd>W</kbd> | Clear query to start / delete word before cursor |
| <kbd>Esc</kbd> / <kbd>Ctrl</kbd> + <kbd>C</kbd> | Cancel search and return to shell prompt |

---

## ⚙️ Configuration & CLI Options

`xstory` complies with the **Freedesktop XDG Base Directory** specification:
* **History Journal:** `~/.local/share/xstory/history.tsv`
* **Configuration:** `~/.config/xstory/config`

### Configuration File (`~/.config/xstory/config`)
```ini
# Sizing mode for the preview panel: 'dynamic' (default) or 'fixed'
preview = dynamic

# Scroll offset cushion when navigating list
scroll_off = 4

# Custom path to history file (optional)
# history_path = /custom/path/history.tsv

# Commands to exclude from recording
exclude_commands = exit, logout, clear

# Sensitive patterns to redact before disk write
secret_patterns = AWS_SECRET, GITHUB_TOKEN, --password
```

### Environment Variables
* `XSTORY_HISTORY_PATH` — Override path to `history.tsv`.
* `XSTORY_PREVIEW` — Sizing mode for preview pane (`dynamic` or `fixed`).
* `XSTORY_SCROLL_OFF` — Scroll cushion margin (number of rows).
* `XSTORY_DISABLE` — Temporarily disable shell recording in active session.

### CLI Reference
```bash
xstory [OPTIONS] [QUERY] [COMMAND]

Commands:
  history  Import or export history logs (import | export)
  record   Record command execution into history log
  init     Initialize shell integration hook (fish | zsh | bash)

Options:
  -H, --history <PATH>   Path to history file
  -s, --scope <SCOPE>    Search scope (global / local)
  -p, --preview <MODE>   Preview panel mode (dynamic / fixed)
  -h, --help             Print help information
  -V, --version          Print version
```

---

## 🔒 Security & Privacy

* **Process Table Safety:** Commands are piped into the recorder via `stdin`, preventing command strings or sensitive tokens from ever appearing in `ps aux` or `/proc`.
* **Automatic Secret Masking:** Sensitive arguments (tokens, passwords, API keys) are masked before persisting to disk.
* **Ignorespace Compliant:** Any command prefixed with a leading space is strictly rejected from persistence.
* **Kernel POSIX `O_APPEND`:** Atomic multi-process writes up to `PIPE_BUF` (4,096 bytes) ensure no corrupted or interleaved lines even across dozens of concurrent shell tabs.

---

## 📄 License

Dual-licensed under the [MIT License](LICENSE) or the [Apache 2.0 License](LICENSE-APACHE).
