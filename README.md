<p align="center">
  <a href="https://github.com/zx0r/xstory/releases">
    <img src="https://img.shields.io/github/v/release/zx0r/xstory?color=4be1ff&label=RELEASE&style=flat-square&logo=github&labelColor=1a1a1a" alt="Release" />
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/LICENSE-MIT%2FApache--2.0-9fef00?style=flat-square&logo=opensourceinitiative&logoColor=white&labelColor=1a1a1a" alt="License" />
  </a>
  <a href="https://www.rust-lang.org/">
    <img src="https://img.shields.io/badge/CORE-RUST_2024-orange?style=flat-square&logo=rust&logoColor=white&labelColor=1a1a1a" alt="Rust 2024" />
  </a>
  <a href="https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html">
    <img src="https://img.shields.io/badge/XDG-COMPLIANT-00FF00?style=flat-square&logo=linux&logoColor=white&labelColor=1a1a1a" alt="XDG Compliant" />
  </a>
</p>

<h2 align="center">
  Minimalist Unix shell history engine & interactive picker
</h2>

<p align="center">
  Columnar memory-mapped arena · Sub-millisecond fuzzy search · Snacks & Lazygit aesthetics
</p>

---

#### Quick Start

##### 1. Installation

```bash
# macOS / Linux (Homebrew)
brew install zx0r/tap/xstory
```

```bash
# Rust Toolchain (Cargo)
cargo install xstory
```

##### 2. Shell Integration

Add to your shell configuration file:

```bash
# Zsh (~/.zshrc)
eval "$(xstory init zsh)"

# Bash (~/.bashrc)
eval "$(xstory init bash)"

# Fish (~/.config/fish/config.fish)
xstory init fish | source
```

---

#### Keybindings (Interactive TUI)

| Shortcut | Action |
|---|---|
| `Ctrl+R` | Toggle search scope: **[G]** Global ↔ **[L]** Current directory |
| `Up` / `Down` | Navigate matching history items |
| `Shift+Up` / `Shift+Down` | Scroll preview pane |
| `Left` / `Right` | Move cursor in query line |
| `Ctrl+U` | Clear search query |
| `Enter` | Execute selected command immediately |
| `Tab` | Insert command into shell buffer for editing |
| `Esc` / `Ctrl+C` | Cancel / exit picker |

---

#### Features

* **Zero-Copy Columnar Arena**: Ingests hundreds of thousands of records in milliseconds with cache-aligned `ByteView` descriptors and 64-bit bitmask early pruning.
* **In-Process Syntax Highlighting**: Direct tree-sitter AST parsing for Bash commands without spawning sub-processes.
* **Context-Aware Scoping**: Switch instantly between global shell history and directory-local history.
* **Strict XDG Compliance**: Data stored at `~/.local/share/xstory/history.tsv`, configuration at `~/.config/xstory/config.toml`.

---

#### CLI Usage

```bash
# Launch interactive search
xstory

# Search with pre-filled query
xstory search "git rebase"

# Import existing history file
xstory history import ~/.zsh_history

# Export history database to stdout
xstory history export
```

---

#### License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
