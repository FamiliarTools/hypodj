//! dj-search-provider - hypodj in GNOME Activities search.
//!
//! Implements `org.gnome.Shell.SearchProvider2` on the session bus. The Shell sends
//! the typed terms; [`provider`] decides what they mean; this file runs the MPD round
//! trips (on the blocking pool - MpdConn is a blocking client) and launches the
//! terminal for the rows that must be confirmed by a human. A down daemon answers
//! with the deterministic rows only, never an error the Shell would log per keystroke.

mod provider;

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hypodj_client::config::{self, Env};
use hypodj_client::mpd::MpdConn;
use hypodj_client::nl::quote_arg;
use zbus::zvariant::{OwnedValue, Value};

use provider::Hit;

const BUS_NAME: &str = "tech.familiartools.HypoDJ.SearchProvider";
const OBJECT_PATH: &str = "/tech/familiartools/HypoDJ/SearchProvider";
/// Library rows per query: the Shell shows a handful per provider anyway.
const LIBRARY_CAP: usize = 5;

/// How long the service stays up with no Shell call before it exits. D-Bus
/// activation starts it again on the next search, so an idle exit costs one spawn and
/// keeps a logged-out or abandoned session from holding a process forever.
const DEFAULT_IDLE: Duration = Duration::from_secs(300);

struct Provider {
    host: String,
    port: u16,
    /// Seconds since `epoch` of the last Shell call, for the idle exit.
    last_call: Arc<AtomicU64>,
    epoch: Instant,
}

impl Provider {
    /// The full result set for a query: deterministic rows first, then library hits.
    fn touch(&self) {
        self.last_call.store(self.epoch.elapsed().as_secs(), Ordering::Relaxed);
    }

    async fn results(&self, terms: Vec<String>) -> Vec<String> {
        self.touch();
        let parsed = provider::parse(&terms);
        let mut hits = parsed.hits;
        if let Some(q) = parsed.library_query {
            let (host, port) = (self.host.clone(), self.port);
            let lib = tokio::task::spawn_blocking(move || {
                let mut c = MpdConn::connect(&host, port).ok()?;
                let pairs = c.command(&format!("searchall {}", quote_arg(&q))).ok()?;
                Some(provider::library_hits(&pairs, LIBRARY_CAP))
            })
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
            hits.extend(lib);
        }
        hits.iter().map(provider::id_of).collect()
    }
}

fn activate(host: &str, port: u16, hit: Hit) {
    match hit {
        Hit::Ask(phrase) => {
            // Echo-before-arm: `djc <phrase>` prints the plan and asks y/N in the
            // terminal. Nothing is ever armed from the search box itself.
            let mut argv = vec![sibling("djc")];
            argv.extend(phrase.split_whitespace().map(str::to_string));
            open_terminal(&argv);
        }
        other => {
            if let Err(e) = run(host, port, &other) {
                eprintln!("dj-search-provider: {e}");
            }
        }
    }
}

fn run(host: &str, port: u16, hit: &Hit) -> Result<(), hypodj_client::mpd::MpdError> {
    let mut c = MpdConn::connect(host, port)?;
    match hit {
        Hit::Command(line) => {
            c.command(line)?;
        }
        Hit::Volume(d) => {
            let status = c.command("status")?;
            let now = field(&status, "volume").and_then(|v| v.parse::<i32>().ok()).unwrap_or(50);
            c.command(&format!("setvol {}", (now + d).clamp(0, 100)))?;
        }
        // Playing an artist means hearing the artist: the daemon's artist radio
        // (hits first, then drift), the same gesture as `r` in dj-gui.
        Hit::Library { uri, .. } if uri.starts_with("artist/") => {
            c.command(&format!("radio {}", quote_arg(uri)))?;
        }
        // An album or song is appended and the new tail played, exactly like
        // dj-gui's play-from-menu: the queue ahead of it is left alone.
        Hit::Library { uri, .. } => {
            let status = c.command("status")?;
            let before = field(&status, "playlistlength").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            c.command(&format!("add {}", quote_arg(uri)))?;
            c.command(&format!("play {before}"))?;
        }
        Hit::Ask(_) => {}
    }
    Ok(())
}

fn field<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A tool shipped next to this binary (same package), falling back to PATH.
fn sibling(name: &str) -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string())
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Run `argv` in a terminal window. `HYPODJ_TERMINAL` names one explicitly (it is
/// invoked as `<term> <argv...>`, the xdg-terminal-exec convention); otherwise the
/// first of xdg-terminal-exec, GNOME Console and GNOME Terminal on PATH.
fn open_terminal(argv: &[String]) {
    let mut cmd = if let Some(t) = std::env::var("HYPODJ_TERMINAL").ok().filter(|t| !t.trim().is_empty()) {
        let mut c = Command::new(t);
        c.args(argv);
        c
    } else if on_path("xdg-terminal-exec") {
        let mut c = Command::new("xdg-terminal-exec");
        c.args(argv);
        c
    } else if on_path("kgx") {
        let mut c = Command::new("kgx");
        c.arg("--").args(argv);
        c
    } else {
        let mut c = Command::new("gnome-terminal");
        c.arg("--").args(argv);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    match cmd.spawn() {
        // Reap it off-thread so a long-lived service never collects zombies.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("dj-search-provider: could not open a terminal: {e}"),
    }
}

#[zbus::interface(name = "org.gnome.Shell.SearchProvider2")]
impl Provider {
    async fn get_initial_result_set(&self, terms: Vec<String>) -> Vec<String> {
        self.results(terms).await
    }

    /// The Shell narrows as the user types. A fresh query is cheap (one local MPD
    /// round trip) and the deterministic rows can change wholesale between "nex" and
    /// "next song", so this re-asks rather than filtering the previous ids.
    async fn get_subsearch_result_set(
        &self,
        _previous_results: Vec<String>,
        terms: Vec<String>,
    ) -> Vec<String> {
        self.results(terms).await
    }

    fn get_result_metas(&self, identifiers: Vec<String>) -> Vec<HashMap<String, OwnedValue>> {
        identifiers
            .iter()
            .inspect(|_| self.touch())
            .filter_map(|id| {
                let hit = provider::hit_of(id)?;
                let (name, description, icon) = provider::meta_of(&hit);
                let mut m = HashMap::new();
                let mut put = |k: &str, v: Value<'_>| {
                    if let Ok(v) = OwnedValue::try_from(v) {
                        m.insert(k.to_string(), v);
                    }
                };
                put("id", Value::from(id.as_str()));
                put("name", Value::from(name.as_str()));
                put("description", Value::from(description.as_str()));
                // `gicon` is g_icon_to_string() text; for a themed icon that is the
                // bare icon name.
                put("gicon", Value::from(icon));
                Some(m)
            })
            .collect()
    }

    async fn activate_result(&self, identifier: String, _terms: Vec<String>, _timestamp: u32) {
        self.touch();
        let Some(hit) = provider::hit_of(&identifier) else { return };
        let (host, port) = (self.host.clone(), self.port);
        let _ = tokio::task::spawn_blocking(move || activate(&host, port, hit)).await;
    }

    /// Clicking the provider's app icon: open the full jukebox.
    fn launch_search(&self, _terms: Vec<String>, _timestamp: u32) {
        self.touch();
        open_terminal(&[sibling("dj-gui")]);
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    for arg in std::env::args().skip(1) {
        if arg == "-h" || arg == "--help" {
            println!(
                "dj-search-provider - hypodj GNOME Shell search provider\n\n\
                 Serves {BUS_NAME} at {OBJECT_PATH} on the session bus.\n\
                 ENV: HYPODJ_HOST/HYPODJ_PORT (daemon), HYPODJ_TERMINAL (terminal launcher)"
            );
            return Ok(());
        }
    }
    let env = Env { get: &|k| std::env::var(k).ok() };
    let (host, port) = config::resolve(None, None, &env);
    let idle = std::env::var("HYPODJ_SEARCH_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE);
    let last_call = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();
    let conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Provider { host, port, last_call: last_call.clone(), epoch })?
        .build()
        .await?;
    // Exit on idle, or as soon as the bus itself is gone (a ping to the bus daemon
    // fails): a session that ended must not leave this process behind.
    let tick = Duration::from_secs(1).min(idle);
    loop {
        tokio::time::sleep(tick).await;
        let quiet = epoch.elapsed().as_secs().saturating_sub(last_call.load(Ordering::Relaxed));
        if quiet >= idle.as_secs() {
            return Ok(());
        }
        // Bounded: a bus that vanished mid-call never answers, it only goes quiet.
        let ping = tokio::time::timeout(
            Duration::from_secs(2),
            conn.call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus.Peer"),
                "Ping",
                &(),
            ),
        )
        .await;
        if !matches!(ping, Ok(Ok(_))) {
            return Ok(());
        }
    }
}
