# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.6] - 2026-10-31

### Fixed
- Replaced generic top-level help stub with dedicated context-specific help screens and usage specifications across all subcommands (`init -h`, `search -h`, `record -h`, `history -h`).

## [0.1.5] - 2026-10-31

### Fixed
- Added `-h` and `--help` flag support across all subcommands (`init -h`, `search -h`, `record -h`, `history -h`).

## [0.1.4] - 2026-10-31

### Added
- Documented `search` subcommand and `-t, --theme` flag in CLI help (`-h`).
- Added architectural scale benchmark suite (`bench_scale`).

## [0.1.3] - 2026-10-31

### Fixed
- Replaced hardcoded version output in `Action::Version` with compile-time `env!("CARGO_PKG_VERSION")`.

## [0.1.2] - 2026-10-31

### Added
- 4K Retina hero banner, demo video, and automated media pipeline (`scripts/build-demo.sh`).
- Cake syntax theme support and interactive `Ctrl+T` theme cycle.
- `cov` coverage recipe in `Justfile` and `.cargo/config.toml` alias.

### Fixed
- Replaced Linux `copy_file_range` syscall with chunked buffered copy in `stream_copy` for Miri compliance.
- Fixed mascot asset path and link references in `README.md`.

## [0.1.1] - 2026-10-30

### Added
- Multi-platform release automation via `cargo-dist` and GitHub Actions.
- Automated Homebrew tap formula publishing for `Xstory`.
- Renovate configuration for dependency updates.
- Dual licensing (`MIT` / `Apache-2.0`) and `CONTRIBUTING.md`.

### Changed
- Configured `[profile.dist]` profile with LTO and symbol stripping.

## [0.1.0] - 2026-10-30

### Added
- Initial release of **xstory** &mdash; a minimalist, high-throughput Unix shell history engine.
- Instant, sub-millisecond full-text search with interactive Ratatui TUI picker.
- Zero-allocation memory-mapped storage arena for high-performance command history.
- Seamless Bash, Zsh, and Fish shell hooks for transparent execution time and exit code tracking.
- Cross-platform distribution pipeline powered by `cargo-dist` with Homebrew tap integration.

[0.1.6]: https://github.com/zx0r/xstory/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/zx0r/xstory/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/zx0r/xstory/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/zx0r/xstory/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/zx0r/xstory/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/zx0r/xstory/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/zx0r/xstory/releases/tag/v0.1.0

