//! `dj-gui --pump <script> <outdir>`: the HEADLESS JOURNEY DRIVER.
//!
//! Runs the real dj-gui render loop - the same workers, the same `handle_key`,
//! `dispatch`, `apply_inbound` - against the real daemon named by the usual
//! `MPD_HOST`/`MPD_PORT` resolution, but with no terminal: keys come from a script,
//! and after each step the driver waits for the async workers to SETTLE, then dumps
//! the rendered frame at 129x40, 80x24 and 60x20 (ratatui `TestBackend`, so no PTY and no
//! display) plus a JSON of daemon truth read on its own socket.
//!
//! This is Approach A (docs/product/agent-driven-surface-tests-2026-10-06.md)
//! lifted out of `#[cfg(test)]` so a journey harness, a CI gate and a cold agent can
//! all drive the SHIPPED binary. Nothing here changes product behaviour: the
//! interactive path never reaches this module.
//!
//! Script, one step per line (`#` comments and blank lines ignored):
//!
//! ```text
//! key <name>       one key: a char, Enter, Esc, Tab, BackTab, Up, Down, Left,
//!                  Right, PageUp, PageDown, Home, End, Backspace, Space, F1..F12,
//!                  or C-<char> for Ctrl
//! type <text>      every char of <text> as its own key
//! say <text>       one sentence on the `:` line: `:`, every char, Enter. ONE input
//!                  as a listener counts it (a journey budget counts it once)
//! wait <secs>      let real time pass (drop timers are wall-clock), then settle
//! exec <cmd>       run `sh -c <cmd>` (the harness flips connectivity this way)
//! snap <label>     settle and dump without sending anything
//! ```
//!
//! Every step, including `exec`, produces `NNN.129x40.txt`, `NNN.80x24.txt`,
//! `NNN.60x20.txt` and `NNN.truth.json` in `<outdir>`, plus one line in `<outdir>/steps.jsonl`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

use hypodj_client::mpd::{MpdConn, MpdError};

use super::{
    apply_inbound, dispatch, request_art, request_card_art, request_grid_art, request_refresh,
    teardown,
    update_sigil, worker, Intent, TuiState,
};

/// The frame sizes every step is rendered at: the classic minimum terminal, a small
/// split pane, and the 129-column rule (the wide rail).
///
/// The WIDE frame is drawn LAST on purpose. Some layout is size-dependent (the Albums
/// cover grid needs room; a small terminal keeps the dense list), and the renderer
/// records what it drew for the keys that follow - one state, three frames - so the
/// last frame decides what the next key means. Last = widest makes a script's keys mean
/// what they mean on a real full-size terminal.
const SIZES: [(u16, u16); 3] = [(80, 24), (60, 20), (129, 40)];
/// Settled = no inbound message for this long.
const QUIET: Duration = Duration::from_millis(600);
/// Give up settling after this long and record `settled: false` - a screen that
/// never settles is itself evidence (a hang with no sign of life).
const SETTLE_CAP: Duration = Duration::from_secs(12);

enum Step {
    Keys(String, Vec<KeyEvent>),
    Wait(f64),
    Exec(String),
    Snap(String),
}

fn parse_key(name: &str) -> Result<KeyEvent, String> {
    let plain = |c| KeyEvent::new(c, KeyModifiers::NONE);
    if let Some(rest) = name.strip_prefix("C-") {
        let mut it = rest.chars();
        return match (it.next(), it.next()) {
            (Some(c), None) => Ok(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
            _ => Err(format!("bad ctrl key {name:?}")),
        };
    }
    let code = match name {
        "Enter" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        "Tab" => KeyCode::Tab,
        "BackTab" => return Ok(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "Backspace" => KeyCode::Backspace,
        "Space" => KeyCode::Char(' '),
        f if f.len() > 1 && f.starts_with('F') => match f[1..].parse::<u8>() {
            Ok(n) => KeyCode::F(n),
            Err(_) => return Err(format!("unknown key {name:?}")),
        },
        s => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => {
                    // An upper-case letter arrives from a real terminal with SHIFT.
                    let m = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
                    return Ok(KeyEvent::new(KeyCode::Char(c), m));
                }
                _ => return Err(format!("unknown key {name:?}")),
            }
        }
    };
    Ok(plain(code))
}

fn parse_script(text: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let step = match verb {
            "key" => Step::Keys(line.to_string(), vec![parse_key(rest.trim())?]),
            "type" => Step::Keys(
                line.to_string(),
                rest.chars().map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)).collect(),
            ),
            "say" => Step::Keys(line.to_string(), {
                let mut k = vec![KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)];
                k.extend(rest.chars().map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
                k.push(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                k
            }),
            "wait" => Step::Wait(rest.trim().parse().map_err(|_| format!("line {}: bad wait", n + 1))?),
            "exec" => Step::Exec(rest.to_string()),
            "snap" => Step::Snap(rest.to_string()),
            _ => return Err(format!("line {}: unknown verb {verb:?}", n + 1)),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// Minimal JSON string escaping (the crate carries no serde).
fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn pairs_json(r: &Result<Vec<(String, String)>, MpdError>) -> String {
    match r {
        Ok(p) => {
            let body: Vec<String> = p.iter().map(|(k, v)| format!("[{},{}]", js(k), js(v))).collect();
            format!("{{\"ok\":true,\"pairs\":[{}]}}", body.join(","))
        }
        Err(e) => format!("{{\"ok\":false,\"error\":{}}}", js(&e.to_string())),
    }
}

/// Daemon truth on the driver's OWN socket (never the TUI's), so a truth read can
/// neither reorder nor unblock what the UI is waiting on. Connectivity is recorded
/// as the daemon itself reports it; there is no verb for that today, so the field
/// says so instead of guessing.
fn truth(host: &str, port: u16, world_file: Option<&Path>) -> String {
    let world = world_file
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| js(s.trim()))
        .unwrap_or_else(|| "null".into());
    let t0 = Instant::now();
    let conn = MpdConn::connect(host, port);
    let (status, current, store, queue) = match conn {
        Ok(mut c) => {
            let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
            (
                c.command("status"),
                c.command("currentsong"),
                c.command("store"),
                c.command("playlistinfo"),
            )
        }
        Err(e) => {
            let err = || Err(MpdError::Io(e.to_string()));
            (err(), err(), err(), err())
        }
    };
    format!(
        "{{\"world\":{world},\"truth_ms\":{},\"status\":{},\"currentsong\":{},\"store\":{},\"playlistinfo\":{},\"daemon_connectivity\":{}}}",
        t0.elapsed().as_millis(),
        pairs_json(&status),
        pairs_json(&current),
        pairs_json(&store),
        pairs_json(&queue),
        js("not exposed by any daemon verb"),
    )
}

fn frame_text(state: &TuiState, w: u16, h: u16) -> String {
    frame_dump(state, w, h).0
}

/// The frame as plain text, plus the same lines each led by a two-character STYLE
/// mark, so a journey can check what plain text cannot carry: `>` when the row holds
/// the cursor (a run of reversed cells), `~` when most of its ink is dimmed (a greyed,
/// inactive row), else a space; then a space and the text.
fn frame_dump(state: &TuiState, w: u16, h: u16) -> (String, String) {
    use ratatui::style::Modifier;
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    let _ = term.draw(|f| super::ui::render(f, state));
    let buf = term.backend().buffer();
    let mut out = String::with_capacity((w as usize + 1) * h as usize);
    let mut marked = String::with_capacity((w as usize + 3) * h as usize);
    for y in 0..h {
        let mut line = String::new();
        let (mut ink, mut dim, mut rev) = (0, 0, 0);
        for x in 0..w {
            let c = &buf[(x, y)];
            line.push_str(c.symbol());
            if c.modifier.contains(Modifier::REVERSED) {
                rev += 1;
            }
            if !c.symbol().trim().is_empty() {
                ink += 1;
                if c.modifier.contains(Modifier::DIM) {
                    dim += 1;
                }
            }
        }
        let line = line.trim_end();
        let mark = if rev >= 3 {
            '>'
        } else if ink > 0 && dim * 2 > ink {
            '~'
        } else {
            ' '
        };
        out.push_str(line);
        out.push('\n');
        marked.push(mark);
        marked.push(' ');
        marked.push_str(line);
        marked.push('\n');
    }
    (out, marked)
}

/// Fold inbound traffic until it has been quiet for [`QUIET`], bounded by
/// [`SETTLE_CAP`]. Returns (settled, elapsed).
fn settle(workers: &worker::Workers, state: &mut TuiState) -> (bool, Duration) {
    let start = Instant::now();
    let mut last_msg = Instant::now();
    loop {
        let mut got = false;
        while let Ok(msg) = workers.inbound_rx.try_recv() {
            apply_inbound(&workers.req_tx, state, msg);
            got = true;
        }
        request_art(&workers.art_tx, state);
        request_card_art(&workers.art_tx, state);
        // The grid records what it SHOWS while it draws, and nothing draws between
        // steps here, so lay the largest frame out once to learn which covers to ask.
        if state.screen == super::state::Screen::Albums {
            let _ = frame_text(state, SIZES[2].0, SIZES[2].1);
        }
        request_grid_art(&workers.grid_art_tx, state);
        update_sigil(state);
        if got {
            last_msg = Instant::now();
        }
        let busy = state.refresh_in_flight;
        if !busy && last_msg.elapsed() >= QUIET {
            return (true, start.elapsed());
        }
        if start.elapsed() >= SETTLE_CAP {
            return (false, start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn run(args: &[String]) -> Result<(), MpdError> {
    let usage = || MpdError::Io("usage: dj-gui --pump <script> <outdir> [world-file]".into());
    let script = args.first().ok_or_else(usage)?;
    let outdir = PathBuf::from(args.get(1).ok_or_else(usage)?);
    let world_file = args.get(2).map(PathBuf::from);
    let text = std::fs::read_to_string(script).map_err(|e| MpdError::Io(format!("{script}: {e}")))?;
    let steps = parse_script(&text).map_err(MpdError::Io)?;
    std::fs::create_dir_all(&outdir).map_err(|e| MpdError::Io(e.to_string()))?;

    let env = hypodj_client::config::Env { get: &|k| std::env::var(k).ok() };
    let (host, port) = hypodj_client::config::resolve(None, None, &env);
    let workers = worker::spawn(&host, port)?;
    let mut state = TuiState::new();
    state.nl_translator = hypodj_client::nl::NlTranslatorPin::from_process_env().map_err(MpdError::Io)?;
    let start = Instant::now();
    request_refresh(&workers.req_tx, &mut state);

    let mut log = std::fs::File::create(outdir.join("steps.jsonl")).map_err(|e| MpdError::Io(e.to_string()))?;
    let mut dump = |n: usize, label: &str, state: &mut TuiState, extra: String| -> Result<(), MpdError> {
        let (settled, took) = settle(&workers, state);
        for (w, h) in SIZES {
            let (text, marked) = frame_dump(state, w, h);
            std::fs::write(outdir.join(format!("{n:03}.{w}x{h}.txt")), text)
                .map_err(|e| MpdError::Io(e.to_string()))?;
            // Not `.txt`: the harness reads `NNN.*x*.txt` as the frames themselves.
            std::fs::write(outdir.join(format!("{n:03}.{w}x{h}.rows")), marked)
                .map_err(|e| MpdError::Io(e.to_string()))?;
        }
        std::fs::write(outdir.join(format!("{n:03}.truth.json")), truth(&host, port, world_file.as_deref()))
            .map_err(|e| MpdError::Io(e.to_string()))?;
        writeln!(
            log,
            "{{\"n\":{n},\"step\":{},\"t_ms\":{},\"settled\":{settled},\"settle_ms\":{}{extra}}}",
            js(label),
            start.elapsed().as_millis(),
            took.as_millis()
        )
        .map_err(|e| MpdError::Io(e.to_string()))?;
        eprintln!("pump: step {n:03} {label} (settled={settled} in {}ms)", took.as_millis());
        Ok(())
    };

    // Step 000: the first screen, before any input.
    dump(0, "start", &mut state, String::new())?;
    let mut quit = false;
    for (i, step) in steps.into_iter().enumerate() {
        let n = i + 1;
        match step {
            Step::Keys(label, keys) => {
                let mut intents = Vec::new();
                for k in keys {
                    if let Some(intent) = state.handle_key(k) {
                        if matches!(intent, Intent::Quit) {
                            quit = true;
                            break;
                        }
                        intents.push(intent);
                    }
                }
                if !intents.is_empty() {
                    dispatch(
                        &workers.req_tx,
                        &workers.cc_tx,
                        &workers.find_tx,
                        &workers.info_tx,
                        &workers.more_tx,
                        &mut state,
                        super::coalesce_intents(intents),
                    );
                }
                dump(n, &label, &mut state, String::new())?;
            }
            Step::Wait(secs) => {
                let until = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
                while Instant::now() < until {
                    while let Ok(msg) = workers.inbound_rx.try_recv() {
                        apply_inbound(&workers.req_tx, &mut state, msg);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                // The interactive loop's 5s safety refresh, which real time would
                // have fired during the wait.
                request_refresh(&workers.req_tx, &mut state);
                dump(n, &format!("wait {secs}"), &mut state, String::new())?;
            }
            Step::Exec(cmd) => {
                let st = std::process::Command::new("sh").arg("-c").arg(&cmd).status();
                let code = st.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
                dump(n, &format!("exec {cmd}"), &mut state, format!(",\"exit\":{code}"))?;
            }
            Step::Snap(label) => dump(n, &format!("snap {label}"), &mut state, String::new())?,
        }
        if quit {
            break;
        }
    }
    teardown(&workers);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_parses_every_verb_and_rejects_unknowns() {
        let s = parse_script("# c\nkey Enter\nkey C-c\nkey G\ntype ab\nwait 1.5\nexec true\nsnap x\nsay skip\n").unwrap();
        assert_eq!(s.len(), 8);
        match &s[7] {
            Step::Keys(_, k) => assert_eq!(k.len(), 6, "colon, four chars, Enter"),
            _ => panic!("say"),
        }
        match &s[1] {
            Step::Keys(_, k) => assert_eq!(k[0], KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            _ => panic!("ctrl key"),
        }
        match &s[3] {
            Step::Keys(_, k) => assert_eq!(k.len(), 2),
            _ => panic!("type"),
        }
        assert!(parse_script("bogus 1").is_err());
        assert!(parse_script("key Nope").is_err());
    }

    #[test]
    fn json_escapes_quotes_and_controls() {
        assert_eq!(js("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
    }
}
