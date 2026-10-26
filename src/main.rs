mod error;

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
