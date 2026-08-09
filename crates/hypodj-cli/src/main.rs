//! dj - the hypodj jukebox CLI. Human-native + natural-language-first: say what
//! you want. A bare control verb (play/pause/stop/next/prev/vol/clear/queue/now)
//! runs directly; anything else is sent to the daemon as `nl "<phrase>"`, echoed,
//! and confirmed y/N. Blocking, one-shot, ONE persistent socket per invocation.

mod heard;
mod pls;
mod render;
mod stations;

use std::io::{BufRead, Write};

use hypodj_client::config::{self, Env};
use hypodj_client::mpd::{MpdConn, MpdError};
use hypodj_client::route::{self, Action};
use hypodj_client::{model, nl};

const HELP: &str = "\
dj - hypodj jukebox

USAGE:
  dj                      show the now-playing card
  dj now | status         show the now-playing card
  dj queue                list the queue
  dj play | pause | stop  playback control
  dj next | prev          skip / go back (also \"next song\", \"skip this\")
  dj fav | favorite       favorite the current track (also \"fav current\")
  dj mark                 mark what is playing: star it if you own it, note it if
                           not, and KEEP THE AUDIO when audio is the only thing
                           that can still help (a radio window off what mpv is
                           already holding - never on a track you own). On a
                           stream whose title JUST changed it records both
                           candidates and stars neither - resolve it with
                           \"mark this\" or \"mark previous\"
  dj heard [all|marks|limit <n>]
                          read the heard ledger back: last session, marks first,
                           unowned only. The first line is the coverage line; a
                           marked row that kept audio carries [tape <n>: ...]
  dj heard keep <n> | drop <n>
                          pin that tape segment against the sweep, or release it
  dj radio                endless radio from what is playing; it keeps going
                           (\"radio random\" to start cold, \"radio off\" to stop)
  dj vol <0-100>          set volume
  dj clear                clear the queue (asks first)
  dj stations import [PATH ...] [--dry-run]
                          save every .pls under PATH as an internet radio
                           station (idempotent: a second run writes nothing).
                           With no PATH, reads $HYPODJ_STATIONS_DIR
  dj stations list | rm <name>
                          list / remove saved stations
  dj <anything else>      natural language: e.g. \"fade out\", \"stop after this
                           album\", \"wake me at 7 with jazz\" - echoed + confirmed

OPTIONS:
  --host <h>    daemon host (default 127.0.0.1)
  --port <p>    daemon port (default 6600, matches the live deploy; a DEV daemon
                defaults to 6601 - point at it with HYPODJ_PORT=6601)
  -h, --help    this help
  -V, --version print version and exit

CONFIG precedence: flags > HYPODJ_HOST/HYPODJ_PORT > MPD_HOST/MPD_PORT
                   > 127.0.0.1:6600
";

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    // --version short-circuits before any socket work. Enriched display version:
    // base semver + commits-since-tag + git short hash on source builds.
    if raw.iter().any(|a| a == "-V" || a == "--version") {
        println!(
            "dj {}",
            hypodj_build_info::version(
                env!("CARGO_PKG_VERSION"),
                option_env!("HYPODJ_BUILD_INFO"),
            )
        );
        return;
    }
    match run(raw) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("dj: {e}");
            std::process::exit(1);
        }
    }
}

/// Parse leading --host/--port/--help flags, leaving the phrase words.
struct Parsed {
    host: Option<String>,
    port: Option<u16>,
    help: bool,
    words: Vec<String>,
}

fn parse_args(raw: Vec<String>) -> Result<Parsed, String> {
    let mut host = None;
    let mut port = None;
    let mut help = false;
    let mut words = Vec::new();
    let mut it = raw.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = Some(it.next().ok_or("--host needs a value")?),
            "--port" => {
                let v = it.next().ok_or("--port needs a value")?;
                port = Some(v.parse::<u16>().map_err(|_| format!("bad port: {v}"))?);
            }
            "-h" | "--help" => help = true,
            // Everything after the first non-flag word is part of the phrase.
            // Flatten each argv element on whitespace so a single quoted arg
            // ("favorite this song") and the unquoted form (favorite this song)
            // both yield the same WORD tokens - route() needs per-word tokens to
            // recognize a bare-favorite phrase, and the TUI already splits this
            // way. Collapsing repeated internal whitespace is fine for the NL
            // reconstruction (route joins the words back with single spaces).
            // `stations` is a FILESYSTEM gesture, not a phrase: its arguments are
            // PATHS, and several of the real .pls files carry spaces in their names.
            // So keep the argv boundaries the shell already established, verbatim,
            // instead of re-splitting a path into fragments that name nothing.
            "stations" => {
                words.push(a);
                words.extend(it.by_ref());
                break;
            }
            _ => {
                words.extend(a.split_whitespace().map(str::to_string));
                for rest in it.by_ref() {
                    words.extend(rest.split_whitespace().map(str::to_string));
                }
                break;
            }
        }
    }
    Ok(Parsed { host, port, help, words })
}

fn run(raw: Vec<String>) -> Result<(), MpdError> {
    let parsed = match parse_args(raw) {
        Ok(p) => p,
        Err(e) => return Err(MpdError::Io(e)),
    };
    if parsed.help {
        print!("{HELP}");
        return Ok(());
    }

    // `stations` is intercepted BEFORE route(): route is deliberately a pure
    // verb-vs-NL split and is SHARED with dj-gui, so a filesystem gesture does not
    // belong in it - and left to the fallthrough, "stations import" would be sent to
    // the NL translator.
    if parsed.words.first().is_some_and(|w| w == "stations") {
        let env = Env { get: &|k| std::env::var(k).ok() };
        let (host, port) = config::resolve(parsed.host, parsed.port, &env);
        let mut conn = MpdConn::connect(&host, port)?;
        // A partial import is reported in full and THEN exits non-zero, so a script
        // sees the failure while the files that did land stay landed.
        if !stations::run(&mut conn, &parsed.words[1..])? {
            std::process::exit(1);
        }
        return Ok(());
    }

    // `heard` is intercepted BEFORE route() for the same stated reason as `stations`:
    // route is deliberately a pure verb-vs-NL split and is SHARED with dj-gui, so it
    // knows only the bare views (`heard`, `heard all`, `heard marks`) plus the two tape
    // pins - and left to the fallthrough, `heard limit 5` and `heard --all` would be
    // handed to the NL translator, which has no heard action at all.
    if parsed.words.first().is_some_and(|w| w == "heard") {
        let env = Env { get: &|k| std::env::var(k).ok() };
        let (host, port) = config::resolve(parsed.host, parsed.port, &env);
        let mut conn = MpdConn::connect(&host, port)?;
        if !heard::run(&mut conn, &parsed.words[1..])? {
            std::process::exit(1);
        }
        return Ok(());
    }

    let action = route::route(&parsed.words);
    if let Action::Help = action {
        print!("{HELP}");
        println!("\n{}", nl::not_understood_hint());
        return Ok(());
    }

    let env = Env { get: &|k| std::env::var(k).ok() };
    let (host, port) = config::resolve(parsed.host, parsed.port, &env);
    let mut conn = MpdConn::connect(&host, port)?;

    match action {
        Action::NowPlaying => print_card(&mut conn)?,
        Action::Queue => {
            let pairs = conn.command("playlistinfo")?;
            println!("{}", render::render_queue(&pairs));
        }
        Action::Command(line) => run_verb(&mut conn, &line)?,
        Action::ClearConfirm => {
            if confirm("clear the whole queue?") {
                conn.command("clear")?;
                print_card(&mut conn)?;
            } else {
                println!("cancelled");
            }
        }
        Action::FavoriteCurrent => favorite_current(&mut conn)?,
        Action::Nl(phrase) => nl_handshake(&mut conn, &phrase)?,
        Action::Help => unreachable!(),
    }
    Ok(())
}

/// Star the currently playing track. The server exposes only
/// `playlistadd Starred <uri>` (no favorite-current shorthand), so resolve the
/// current song's uri from `currentsong` first.
///
/// A raw stream is not a library song and has no star surface of its own, so the
/// gesture goes to the daemon's `mark` verb, which holds everything the decision needs
/// (the previous ICY title, the subject ages, the provenance-stamped library match, the
/// ledger) and answers with the one sentence a human reads: it stars the local copy
/// when the subject is owned and unambiguous, notes a pointer row when it is not, and
/// refuses to pick between two plausible subjects rather than guessing one.
fn favorite_current(conn: &mut MpdConn) -> Result<(), MpdError> {
    let current = conn.command("currentsong")?;
    let np = model::now_playing(&[], &current);
    let uri = match np.file.as_deref() {
        Some(u) => u,
        None => {
            println!("nothing is playing to favorite");
            return Ok(());
        }
    };
    if !uri.starts_with("song/") {
        return run_verb(conn, "mark");
    }
    let uri = uri.to_string();
    let uri = uri.as_str();
    match conn.command(&format!("playlistadd Starred {uri}")) {
        Ok(_) => {
            let label = np.title.as_deref().unwrap_or(uri);
            println!("favorited: {label}");
            print_card(conn)?;
        }
        Err(MpdError::Ack(msg)) => println!("could not favorite: {msg}"),
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Run one command line and print the card - plus, first, whatever the verb ANSWERED.
/// Most MPD verbs answer with a bare OK and the card is the whole feedback, but `mark`
/// returns `mark_result`, the one sentence a human reads. Dropping it would make
/// `dj mark` succeed in silence, which is the precise bug the gesture exists to end.
fn run_verb(conn: &mut MpdConn, line: &str) -> Result<(), MpdError> {
    let pairs = conn.command(line)?;
    if let Some((_, sentence)) = pairs.iter().find(|(k, _)| k == "mark_result") {
        println!("{sentence}");
    }
    print_card(conn)
}

/// Fetch status + currentsong on the SAME connection and print the card.
fn print_card(conn: &mut MpdConn) -> Result<(), MpdError> {
    let status = conn.command("status")?;
    let current = conn.command("currentsong")?;
    let np = model::now_playing(&status, &current);
    println!("{}", render::render_card(&np));
    Ok(())
}

/// The full NL handshake, all on the one open socket. Under `cc` (and only when the
/// `claude` CLI is present) the phrase is first translated CLIENT-SIDE by Claude
/// Code into a validated Plan IR, echoed + confirmed here, and armed via a normal
/// `plan add <dsl>` (re-clamped + dry-run validated daemon-side, the same trust
/// boundary as `nl confirm`). When `cc` is off, `claude` is absent, or the call
/// fails, it falls through to today's daemon `nl` path unchanged.
fn nl_handshake(conn: &mut MpdConn, phrase: &str) -> Result<(), MpdError> {
    // NOTE: the latent-field pull is set DAEMON-SIDE at the confirmed enqueue
    // (`plan_enqueue`), never speculatively primed here before the user confirms - a
    // rejected or non-enqueue ask must never leave a lingering bias behind.
    #[cfg(feature = "cc")]
    {
        if hypodj_nl::cc::cc_available() {
            match cc_nl_handshake(conn, phrase)? {
                true => return Ok(()),
                // The CC call failed (spawn/parse/no-DSL); fall through to the daemon.
                false => {}
            }
        }
    }
    let req = nl::nl_request(phrase);
    let pairs = match conn.command(&req) {
        Ok(p) => p,
        // An ACK here is a translate failure: map to a friendly reason.
        Err(MpdError::Ack(msg)) => {
            println!("{}", nl::map_ack_reason(&msg));
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let token = match nl::token_from_pairs(&pairs) {
        Some(t) => t,
        None => {
            println!("the server did not return a plan to confirm");
            return Ok(());
        }
    };

    if let Some(echo) = nl::echo_from_pairs(&pairs) {
        let parts = nl::split_echo(&echo);
        if let Some(trust) = &parts.trust {
            println!("({trust})");
        }
        for step in &parts.steps {
            println!("{step}");
        }
        // Wake caveat surfaced as a warning ABOVE the prompt.
        if let Some(note) = &parts.note {
            println!("\n! {note}");
        }
    }

    if confirm("confirm?") {
        match conn.command(&format!("nl confirm {token}")) {
            Ok(plan_pairs) => {
                for (k, v) in &plan_pairs {
                    if k == "plan_id" {
                        println!("{}", nl::armed_line(v));
                    }
                }
                print_card(conn)?;
            }
            Err(MpdError::Ack(msg)) => println!("{}", nl::map_ack_reason(&msg)),
            Err(e) => return Err(e),
        }
    } else {
        // Best-effort cancel on the open connection before exiting.
        let _ = conn.command(&format!("nl cancel {token}"));
        println!("cancelled");
    }
    Ok(())
}

/// The Claude Code client-side NL handshake. Reads the small context the client
/// already has (queue length, is-playing) from `status`, then makes ONE non-streamed
/// `claude` call (`--output-format json`): a simple "thinking..." indicator on stderr
/// keeps the multi-second call from looking frozen (stdout stays clean for the echo +
/// confirm), and the settled VALIDATED RawPlan is echoed via describe_plan, confirmed
/// y/N, and armed via `plan add <dsl>`. Returns Ok(true) when it handled the phrase
/// (armed, cancelled, or a loud user-facing miss), Ok(false) to fall through to the
/// daemon `nl` path (spawn/parse failure, or a plan not DSL-expressible).
#[cfg(feature = "cc")]
fn cc_nl_handshake(conn: &mut MpdConn, phrase: &str) -> Result<bool, MpdError> {
    let status = conn.command("status")?;
    let queue_len = status
        .iter()
        .find(|(k, _)| k == "playlistlength")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let is_playing = status
        .iter()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v == "play")
        .unwrap_or(false);

    // GROUND the prompt with REAL library candidates so Claude picks a genre/artist/
    // query that ACTUALLY exists instead of guessing a blind string. Both reads run on
    // this same socket BEFORE the claude call and DEGRADE CLEANLY (empty on any MPD
    // error) - an empty context reproduces today's un-grounded prompt, never a failure.
    let ctx = hypodj_nl::cc::LibraryContext {
        genres: hypodj_client::grounding::list_genres(conn),
        candidates: hypodj_client::grounding::search_labels(conn, phrase, 20),
        notes: Vec::new(),
    };

    // Simple "thinking..." indicator on stderr (stdout stays clean for the echo +
    // y/N). The blocking multi-second call is fine here - the CLI is a one-shot; the
    // indicator keeps it from ever looking frozen. One non-streamed call returns the
    // settled VALIDATED plan directly (the installed CLI returns the result intact).
    eprint!("Claude Code: thinking...");
    let _ = std::io::stderr().flush();
    let result = hypodj_nl::cc::run_claude(phrase, queue_len, is_playing, &ctx);
    // Clear the indicator line before any output.
    eprint!("\r\x1b[2K");
    let _ = std::io::stderr().flush();

    let raw = match result {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("Claude Code could not translate that ({e}); trying the built-in translator.");
            return Ok(false);
        }
    };

    let dsl = match hypodj_nl::render_dsl(&raw) {
        Some(d) => d,
        None => {
            // A validated plan the keyword DSL cannot express (e.g. time_remaining);
            // fall through so the daemon rules can have a go.
            return Ok(false);
        }
    };

    println!("(via Claude Code)");
    println!("{}", hypodj_nl::describe_plan(&raw));

    if confirm("confirm?") {
        match conn.command(&format!("plan add {dsl}")) {
            Ok(plan_pairs) => {
                // Prefer the REAL execute-time outcome ("added N", "added 0 - no
                // matches for X", "played X") the daemon returns for an immediate
                // plan; fall back to the plan-time armed line for a deferred plan.
                match nl::result_line_from_pairs(&plan_pairs) {
                    Some(line) => println!("{line}"),
                    None => {
                        for (k, v) in &plan_pairs {
                            if k == "plan_id" {
                                println!("{}", nl::armed_line(v));
                            }
                        }
                    }
                }
                print_card(conn)?;
            }
            Err(MpdError::Ack(msg)) => println!("{}", nl::map_ack_reason(&msg)),
            Err(e) => return Err(e),
        }
    } else {
        println!("cancelled");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_splits_single_quoted_phrase_into_word_tokens() {
        // `dj "favorite this song"` arrives as ONE argv element; it must flatten to
        // per-word tokens so route() sees the bare-favorite phrase (the live miss).
        let p = parse_args(v(&["favorite this song"])).unwrap();
        assert_eq!(p.words, v(&["favorite", "this", "song"]));
        assert_eq!(route::route(&p.words), Action::FavoriteCurrent);
    }

    #[test]
    fn parse_args_keeps_station_paths_whole() {
        // `stations` takes PATHS, not phrase words. Several of the real .pls files carry
        // spaces and a comma in their names, so re-splitting an argv element on
        // whitespace would turn one real path into fragments that name nothing.
        let p = parse_args(v(&[
            "stations",
            "import",
            "/home/u/radio streams/Moon Mission Recordings, Tokyo Deep and Electronic.pls",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(
            p.words,
            v(&[
                "stations",
                "import",
                "/home/u/radio streams/Moon Mission Recordings, Tokyo Deep and Electronic.pls",
                "--dry-run",
            ])
        );
        // The leading flags still parse, and `stations` still reaches the gesture.
        let p = parse_args(v(&["--port", "6612", "stations", "import", "/a b/c"])).unwrap();
        assert_eq!(p.port, Some(6612));
        assert_eq!(p.words, v(&["stations", "import", "/a b/c"]));
    }

    #[test]
    fn parse_args_unquoted_words_match_quoted_form() {
        // Quoted and unquoted phrasings produce IDENTICAL tokens on both surfaces.
        let quoted = parse_args(v(&["fav current music"])).unwrap();
        let unquoted = parse_args(v(&["fav", "current", "music"])).unwrap();
        assert_eq!(quoted.words, unquoted.words);
        assert_eq!(quoted.words, v(&["fav", "current", "music"]));
        assert_eq!(route::route(&quoted.words), Action::FavoriteCurrent);
    }

    #[test]
    fn parse_args_collapses_internal_whitespace_for_nl() {
        // Repeated internal whitespace collapses; NL reconstruction stays clean.
        let p = parse_args(v(&["wake me   at 7  with jazz"])).unwrap();
        assert_eq!(
            route::route(&p.words),
            Action::Nl("wake me at 7 with jazz".into())
        );
    }

    #[test]
    fn heard_must_be_intercepted_because_route_would_send_it_to_the_translator() {
        // This is WHY `heard` is claimed before route(), exactly as `stations` is:
        // route is a pure verb-vs-NL split SHARED with dj-gui, so it knows only the
        // bare views. Every argument-carrying form falls through to NL, where the
        // question about last night's listening would come back as a phrasing hint.
        assert_eq!(
            route::route(&v(&["heard", "limit", "5"])),
            Action::Nl("heard limit 5".into())
        );
        assert_eq!(route::route(&v(&["heard", "--all"])), Action::Nl("heard --all".into()));
        // And the tokens survive parse_args intact, so the interception sees them.
        let p = parse_args(v(&["heard", "--all"])).unwrap();
        assert_eq!(p.words, v(&["heard", "--all"]));
        assert_eq!(p.words.first().map(String::as_str), Some("heard"));
        let p = parse_args(v(&["--port", "6699", "heard", "limit", "5"])).unwrap();
        assert_eq!(p.port, Some(6699));
        assert_eq!(p.words, v(&["heard", "limit", "5"]));
        // The bare views DO route, which is what makes `:heard` work in the TUI.
        assert_eq!(route::route(&v(&["heard"])), Action::Command("heard".into()));
        assert_eq!(route::route(&v(&["heard", "all"])), Action::Command("heard all".into()));
    }

    #[test]
    fn parse_args_flags_still_parse_before_phrase() {
        let p = parse_args(v(&["--host", "example", "--port", "6601", "next song"])).unwrap();
        assert_eq!(p.host.as_deref(), Some("example"));
        assert_eq!(p.port, Some(6601));
        assert_eq!(p.words, v(&["next", "song"]));
    }
}

/// A default-No y/N prompt. Only "y"/"yes" (case-insensitive) confirm; bare
/// Enter, "n", EOF (Ctrl-D), all mean No.
pub(crate) fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => false, // EOF
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}
