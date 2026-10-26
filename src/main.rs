mod arena;
mod error;
#[allow(dead_code)]
mod record;
#[allow(dead_code)]
mod search;
#[allow(dead_code)]
mod syntax;
#[allow(dead_code)]
mod tui;

use error::Error;

fn main() {
    if let Err(e) = run() {
        eprintln!("xstory: {e}");
        std::process::exit(e.exit_code());
    }
}

#[allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)]
fn run() -> Result<(), Error> {
    // Step 7 will populate CLI dispatch here.
    Ok(())
}
