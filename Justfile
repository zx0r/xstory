default: test

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

build:
    cargo build --release

test:
    cargo test

lint: fmt-check
    cargo clippy --all-targets -- -D warnings

bench:
    cargo bench -- --save-baseline main
