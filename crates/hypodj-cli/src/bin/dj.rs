//! `dj` - the listening shell. On an interactive terminal with no arguments it
//! becomes `dj-gui` (the TUI); with arguments, or when stdin/stdout is not a
//! terminal (a script, a pipe, an agent), it is exactly `djc`, the jukebox CLI.

use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

fn main() {
    let has_args = std::env::args_os().len() > 1;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !has_args && interactive {
        // exec only returns on failure; then the CLI card is still a useful answer.
        let err = std::process::Command::new(gui_path()).exec();
        eprintln!("dj: could not start dj-gui ({err}); showing the djc card instead");
    } else if has_args {
        if let Some(notice) = hypodj_cli::rename_notice(env!("CARGO_PKG_VERSION")) {
            eprintln!("{notice}");
        }
    }
    hypodj_cli::main_as("dj");
}

/// `dj-gui` beside this binary (the nix package installs them together), else
/// whatever `dj-gui` is on PATH.
fn gui_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("dj-gui")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("dj-gui"))
}
