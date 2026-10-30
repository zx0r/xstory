# Justfile - Systems Engineering Command Orchestrator
# Architecture: Minimalist Unix Shell History Engine

default: qa

# ==============================================================================
# Domain 1: Cargo Development Lifecycle
# ==============================================================================

# Format code according to rustfmt.toml
fmt:
    cargo fmt --all

# Check formatting compliance without writing
fmt-check:
    cargo fmt-check

# Run complete static analysis & lint suite (Pedantic/Nursery)
qa: fmt-check
    cargo qa

# Run all process-isolated tests
test:
    cargo t

# Run end-to-end integration tests
e2e:
    cargo e2e

# Run source-based LLVM coverage suite
coverage:
    @mkdir -p target/llvm-cov
    cargo llvm-cov nextest --profile ci --lcov --output-path target/llvm-cov/lcov.info

# ==============================================================================
# Domain 2: Production Artifacts & Distribution
# ==============================================================================

# Build size-optimized micro-binary (< 380 KB)
release-small:
    cargo build --profile release-small
    @ls -lh target/release-small/xstory

# Build full-throughput production release
release:
    cargo build --release

# Run local cargo-dist release packaging dry run
dist-plan:
    dist plan

# ==============================================================================
# Domain 3: Supply Chain & Workflow Verification
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
    brew install bacon sccache cargo-deny cargo-llvm-cov cargo-dist cargo-mutants cargo-binstall lefthook cocogitto actionlint zizmor gitleaks
    cargo binstall -y cargo-machete cargo-semver-checks
    rustup toolchain install nightly --component miri,rust-src
