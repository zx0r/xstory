# Justfile - Systems Engineering Command Orchestrator
# Architecture: Minimalist Unix Shell History Engine

set dotenv-load := false
set export := false
set shell := ["bash", "-euo", "pipefail", "-c"]

bin := "xstory"
target_dir := "target"

default: qa

# ==============================================================================
# Domain 1: Cargo Development Lifecycle
# ==============================================================================

# Format code in-place according to rustfmt.toml
fmt:
    cargo fmt --all

# Strict read-only check for CI & Git hooks (fails on diff)
fmt-check:
    cargo fmt --all -- --check

# Run complete verification suite (style check + pedantic/nursery lints)
qa: fmt-check
    cargo clippy --all-targets --all-features -- -D warnings

# Run all process-isolated tests via nextest (supports: just test <filter>)
test *args:
    cargo nextest run --all-targets {{ args }}

# Run tests via standard cargo test harness (supports: just test-all <filter>)
test-all *args:
    cargo test --all-targets --all-features {{ args }}

# Run end-to-end integration tests
e2e:
    cargo test --test e2e

# Run performance benchmarks via Criterion (supports: just bench <filter>)
bench *args:
    cargo bench {{ args }}

# Run scale benchmark suite (500k records, 120Hz SLA verification)
bench-scale:
    cargo bench --bench bench_scale

# Run comparative stress benchmark: xstory vs Atuin (100k/500k/1M, zero hardcode)
bench-comparative:
    cargo bench --bench bench_comparative

# Run comparative system benchmark against Atuin (size, SLOC, RSS, hardware counters)
bench-system:
    python3 scripts/benchmark.py

# Run source-based LLVM coverage suite
cov:
    @mkdir -p {{ target_dir }}/llvm-cov
    cargo llvm-cov nextest --profile ci --lcov --output-path {{ target_dir }}/llvm-cov/lcov.info

coverage: cov

# Continuous background compilation & testing
watch:
    bacon

# ==============================================================================
# Domain 2: Production Artifacts & Distribution
# ==============================================================================

# Build size-optimized release binary (opt-level = "z", strip = true)
release-small:
    cargo build --profile release-small
    @ls -lh {{ target_dir }}/release-small/{{ bin }}

# Build full-throughput production release (opt-level = 3, LTO fat)
release:
    cargo build --release
    @ls -lh {{ target_dir }}/release/{{ bin }}

# Run local cargo-dist release packaging dry run
dist-plan:
    dist plan

# Run automated media assets pipeline (4K banner + video + GIF)
assets *args:
    ./scripts/build-demo.sh {{ args }}

# ==============================================================================
# Domain 3: DevSecOps, Supply Chain & Workflows
# ==============================================================================

# Run supply chain security audit (RustSec, bans, licenses)
audit:
    cargo deny check

# Scan repository history and working tree for leaked secrets
audit-secrets:
    gitleaks detect --no-banner

# Scan for unused dependencies across all targets
machete:
    cargo machete

# Validate GitHub Actions workflows locally (actionlint + zizmor offline)
lint-workflows:
    actionlint -color
    zizmor --offline --min-severity high .github/workflows/

# Run memory safety & aliasing verification via Miri Tree Borrows
miri:
    env MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test --lib

# Verify SemVer API backwards compatibility
semver:
    cargo semver-checks check-release

# Run mutation testing suite
mutants:
    cargo mutants

# Single-command composite gate (equivalent to local CI pass)
check-all: qa test audit audit-secrets machete lint-workflows

# ==============================================================================
# Domain 4: Tooling & Git Hooks
# ==============================================================================

# Install repository Git hooks via Lefthook
hooks-install:
    lefthook install

# Clean build artifacts
clean:
    cargo clean

# Install complete developer toolchain into workstation
setup-tools:
    brew install bacon sccache cargo-deny cargo-llvm-cov cargo-nextest cargo-dist cargo-mutants cargo-binstall lefthook cocogitto actionlint zizmor gitleaks
    cargo binstall -y cargo-machete cargo-semver-checks
    rustup toolchain install nightly --component miri,rust-src
