default: test

build:
    cargo build --release

test:
    cargo test

lint:
    cargo clippy --all-targets -- -D warnings

bench:
    cargo bench -- --save-baseline main
