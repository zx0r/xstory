<p align="center">
  <a href="https://github.com/zx0r/xstory/releases">
    <img src="https://img.shields.io/github/v/release/zx0r/xstory?color=4be1ff&label=RELEASE&style=flat-square&logo=github&labelColor=1a1a1a" />
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/LICENSE-MIT%20%2F%20Apache--2.0-9fef00?style=flat-square&logo=opensourceinitiative&logoColor=white&labelColor=1a1a1a" />
  </a>
  <a href="https://www.rust-lang.org/">
    <img src="https://img.shields.io/badge/CORE-RUST-orange?style=flat-square&logo=rust&logoColor=white&labelColor=1a1a1a" />
  </a>
  <a href="https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html">
    <img src="https://img.shields.io/badge/XDG-COMPLIANT-00FF00?style=flat-square&logo=linux&logoColor=white&labelColor=1a1a1a" />
  </a>
</p>

<h2 align="center">
  Minimalist Unix shell history engine
</h2>

<p align="center">
  Fast, transparent history recorder and fuzzy finder for Fish, Zsh, and Bash
</p>

---

#### Quick Start

##### 1. Installation

```bash
# macOS / Linux (Homebrew)
brew install zx0r/tap/xstory
```

```bash
# Cargo
cargo install xstory
```

##### 2. Shell Integration

**Fish** (`~/.config/fish/config.fish`):
```fish
xstory init fish | source
```

**Zsh** (`~/.zshrc`):
```zsh
eval "$(xstory init zsh)"
```

**Bash** (`~/.bashrc`):
```bash
eval "$(xstory init bash)"
```

##### 3. Import History (Optional)

```bash
# Automatically detects and imports legacy history from Fish, Zsh, or Bash
xstory history import
```

---

#### Overview

* **Zero-copy Indexing:** Memory-mapped log (`memmap2`) with in-memory string arena.
* **Fuzzy Search:** Sub-millisecond matching powered by `nucleo-matcher`.
* **Context Tracking:** Records execution duration, exit code, and working directory (`$PWD`).
* **Syntax Highlighting:** Real-time multi-line highlighting via Tree-sitter (Bash AST).
* **Privacy:** Automatic masking of tokens, passwords, and sensitive keys before disk write.
* **XDG Storage:** History log stored at `~/.local/share/xstory/history.tsv`.

---

#### Usage

* `Up` or `Ctrl-R` — Trigger interactive history search
* `Ctrl-S` — Toggle between **Global** (all history) and **Local** (current directory) scopes
* `Tab` — Put command into prompt without executing
* `Enter` — Execute command immediately

---

#### License

Dual-licensed under [MIT](LICENSE) or [Apache-2.0](LICENSE-APACHE).
