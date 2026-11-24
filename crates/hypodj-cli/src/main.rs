//! `djc` - the hypodj jukebox CLI. All of it lives in the library so the `dj`
//! listening shell can fall back to exactly the same code.

fn main() {
    hypodj_cli::main();
}
