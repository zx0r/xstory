# Justfile - Enterprise Engineering Command Orchestrator
# Level: Principal Systems Architect

default: qa

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

# Build size-optimized micro-binary (< 380 KB)
release-small:
    cargo build --profile release-small
    @ls -lh target/release-small/xstory

# Build full-throughput production release
release:
    cargo build --release

# Run supply chain security audit (RustSec, bans, licenses)
audit:
    cargo deny check

# Run source-based LLVM coverage suite
coverage:
    cargo llvm-cov nextest --profile ci --lcov --output-path target/llvm-cov/lcov.info

# Run local cargo-dist release packaging dry run
dist-plan:
    dist plan

# Clean build artifacts
clean:
    cargo clean

# Scan for unused dependencies across all targets
machete:
    cargo machete

# Run memory safety & aliasing verification via Miri Tree Borrows
miri:
    env MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test --lib

# Install complete Principal developer toolchain into workstation
setup-tools:
    brew install bacon sccache cargo-deny cargo-llvm-cov cargo-dist cargo-mutants cargo-binstall
    cargo binstall -y cargo-machete cargo-semver-checks
    rustup toolchain install nightly --component miri,rust-src
