//! `dj stations` - the gesture that makes a curated `.pls` collection first-class.
//!
//! The FILES are read here, on the human's side of the socket, and the IDEMPOTENCE
//! lives in the daemon's `station add` verb. That split is deliberate: the daemon does
//! read files, but always its OWN state from its own config - never a path a client
//! named - and a filesystem gesture typed at a shell wants tab-completion, `~`
//! expansion and globs, none of which exist inside an MPD protocol line. Meanwhile
//! putting the upsert rule daemon-side means a second import writes nothing no matter
//! which client drives it, or whether two runs overlap.
//!
//! So this module is a loop: parse a `.pls`, send one `station add "<url>" "<name>"`,
//! print what the daemon says it did. 22 files is 22 commands - one protocol shape
//! instead of a whole batch grammar for the sake of a second saved.
//!
//! The one thing the daemon CANNOT decide from a single call is whether two entries of
//! the SAME run are fighting over a key (a multi-entry `.pls` with no `TitleN`, two files
//! sharing a `Title1`), because each call only sees the stations that exist at that
//! instant. That is the run's own job, and it lives in [`run_clash`].

use std::path::{Path, PathBuf};

use hypodj_client::mpd::{MpdConn, MpdError};
use hypodj_client::nl::quote_arg;

/// What the import decided (or, under `--dry-run`, would decide) for one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing saved carries this url or this name.
    Create,
    /// One saved station is this station under a stale url or a stale name; the
    /// payload is its previous url when the url is what changed.
    Update(Option<String>),
    /// This exact (url, name) pair is already saved: zero writes.
    Unchanged,
    /// The url belongs to one saved station while the name belongs to a different one.
    Conflict,
}

/// Run the `stations` gesture. `words` are the argv tokens AFTER the leading
/// `stations`, kept as separate argv elements so a path containing a space (several of
/// the real `.pls` FILES have one) is never split.
///
/// Returns `Ok(true)` when everything succeeded. The caller maps `Ok(false)` to a
/// non-zero exit, so a partial import is visible to a script without being fatal to the
/// files that did land.
pub fn run(conn: &mut MpdConn, words: &[String]) -> Result<bool, MpdError> {
    match words.first().map(|s| s.as_str()) {
        Some("import") => import(conn, &words[1..]),
        Some("rm") if words.len() == 2 => rm(conn, &words[1]),
        Some("list") if words.len() == 1 => list(conn),
        _ => {
            eprintln!("{USAGE}");
            Ok(false)
        }
    }
}

const USAGE: &str = "\
usage:
  dj stations import [PATH ...] [--dry-run]   save every .pls under PATH as a station
  dj stations list                            list the saved stations
  dj stations rm <name>                       remove one saved station

With no PATH, import reads $HYPODJ_STATIONS_DIR. A PATH may be a directory (its
.pls files, not recursive) or individual .pls files. Importing is idempotent: a
second run over the same files writes nothing.";

/// `dj stations list` - the saved set, straight from the daemon's `Stations` browse dir.
fn list(conn: &mut MpdConn) -> Result<bool, MpdError> {
    let saved = saved_stations(conn)?;
    if saved.is_empty() {
        println!("no saved stations");
        return Ok(true);
    }
    let width = saved.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0);
    for (name, url) in &saved {
        println!("{name:<width$}  {url}");
    }
    println!("{} station{}", saved.len(), if saved.len() == 1 { "" } else { "s" });
    Ok(true)
}

/// `dj stations rm <name>` - the undo half. A gesture that writes to the user's server
/// must be removable by the same tool.
fn rm(conn: &mut MpdConn, name: &str) -> Result<bool, MpdError> {
    // Deleting from the user's own Navidrome is destructive and not undoable from
    // here, so it asks first - the same bar `dj clear` already sets for the queue.
    // A station is harder to get back than a queue: the .pls it came from may be the
    // only record of the URL.
    if !crate::confirm(&format!("remove the saved station {name:?}?")) {
        println!("cancelled  {name}");
        return Ok(false);
    }
    match conn.command(&format!("station rm {}", quote_arg(name))) {
        Ok(pairs) => {
            let canonical = pair(&pairs, "Name").unwrap_or_else(|| name.to_string());
            println!("removed    {canonical}");
            Ok(true)
        }
        Err(MpdError::Ack(msg)) => {
            println!("failed     {name}: {msg}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// `dj stations import [PATH ...] [--dry-run]`.
fn import(conn: &mut MpdConn, args: &[String]) -> Result<bool, MpdError> {
    let mut dry_run = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    for a in args {
        match a.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            other => paths.push(PathBuf::from(shell_expand_home(other))),
        }
    }
    if paths.is_empty() {
        let env = hypodj_client::config::Env { get: &|k| std::env::var(k).ok() };
        match hypodj_client::config::stations_dir(&env) {
            Some(d) => paths.push(d),
            None => {
                eprintln!(
                    "dj stations import: no PATH given and HYPODJ_STATIONS_DIR is not set"
                );
                return Ok(false);
            }
        }
    }

    let files = match collect_pls_files(&paths) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("dj stations import: {e}");
            return Ok(false);
        }
    };
    if files.is_empty() {
        println!("no .pls files found");
        return Ok(true);
    }

    // A dry run decides CLIENT-side from one read of the saved set and sends zero
    // writes. The daemon stays the authority - it re-decides on the real write - so
    // this is a preview, never a substitute.
    let saved = if dry_run { Some(saved_stations(conn)?) } else { None };

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    let mut unknown = 0usize;
    let mut failed = 0usize;
    // The (name, url) pairs this run has already sent. Both are per-station unique keys
    // on the server, and the daemon only ever sees one call at a time, so keeping them
    // unique across the WHOLE run is the importer's job - see [`run_clash`].
    let mut claimed: Vec<(String, String)> = Vec::new();

    for (idx, file) in files.iter().enumerate() {
        let label = file.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string();
        // Fail LOUD on invalid UTF-8 rather than lossy-converting: a mangled name is a
        // broken idempotency key, and a broken key mints a duplicate on every run.
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                println!("failed     {label}: {e}");
                failed += 1;
                continue;
            }
        };
        let entries = crate::pls::parse_pls(&text);
        if entries.is_empty() {
            println!("failed     {label}: no playable entry (no FileN url)");
            failed += 1;
            continue;
        }
        if let Some(declared) = crate::pls::declared_entry_count(&text) {
            if declared != entries.len() {
                println!(
                    "note       {label}: NumberOfEntries says {declared}, found {}",
                    entries.len()
                );
            }
        }
        let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or(&label).to_string();
        for entry in entries {
            let (name, from_stem) = match entry.title {
                Some(t) => (t, false),
                // The FILE STEM, not the url: the daemon's derived-label path already
                // over-produces urls, and a url is not a name a human can type.
                None => (stem.clone(), true),
            };
            let suffix = if from_stem { "  (no TitleN, used file name)" } else { "" };
            // Caught BEFORE the write, and identically under `--dry-run`: an entry that
            // fights an earlier one over a key is refused here rather than sent, because
            // the daemon would read it as a rename of the row that entry just saved - a
            // stream silently dropped and an import that rewrites his server on every
            // single run instead of converging.
            match run_clash(&claimed, &entry.url, &name) {
                Clash::None => claimed.push((name.clone(), entry.url.clone())),
                Clash::Duplicate => {
                    println!("duplicate  {name}{suffix}  (listed twice, already saved above)");
                    unchanged += 1;
                    continue;
                }
                // Both messages name the url that did NOT get saved: the whole point of
                // refusing is that the dropped stream is visible, and actionable.
                Clash::Name(prev_url) => {
                    println!(
                        "failed     {name}: not saving {} - this run already saved {prev_url} \
                         under that name{}",
                        entry.url,
                        if from_stem {
                            " (give each entry its own TitleN)"
                        } else {
                            " (two entries need two names)"
                        }
                    );
                    failed += 1;
                    continue;
                }
                Clash::Url(prev_name) => {
                    println!(
                        "failed     {name}: not saving this name - this run already saved {} \
                         as \"{prev_name}\"",
                        entry.url
                    );
                    failed += 1;
                    continue;
                }
            }
            let outcome = match &saved {
                Some(saved) => {
                    let v = preview_verdict(saved, &entry.url, &name);
                    report_preview(&v, &name, &entry.url, suffix)
                }
                None => match send_add(conn, &entry.url, &name, suffix)? {
                    Some(o) => o,
                    // A transport failure makes every subsequent send pointless: stop,
                    // and say which files never got a chance.
                    None => {
                        let remaining = files.len() - idx - 1;
                        if remaining > 0 {
                            println!("skipped    {remaining} file(s): connection lost");
                        }
                        return Ok(false);
                    }
                },
            };
            match outcome {
                Reported::Created => created += 1,
                Reported::Updated => updated += 1,
                Reported::Unchanged => unchanged += 1,
                Reported::Unknown => unknown += 1,
                Reported::Failed => failed += 1,
            }
        }
    }

    println!(
        "{} file{}: {}",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        summary(dry_run, created, updated, unchanged, unknown, failed)
    );
    // An UNKNOWN outcome is not a success. The run wrote something the client could not
    // confirm, so the exit status must not claim a clean import.
    Ok(failed == 0 && unknown == 0)
}

/// The tally line. Only non-zero counts appear (a run of 22 unchanged should not have to
/// be read past three zeros), and a run that did literally nothing says so rather than
/// printing an empty list. Under `--dry-run` the write verbs are conditional, because
/// nothing was written.
fn summary(
    dry_run: bool,
    created: usize,
    updated: usize,
    unchanged: usize,
    unknown: usize,
    failed: usize,
) -> String {
    let word = |would: &str, did: &str| if dry_run { format!("would {would}") } else { did.to_string() };
    let mut parts = Vec::new();
    if created > 0 {
        parts.push(format!("{created} {}", word("create", "created")));
    }
    if updated > 0 {
        parts.push(format!("{updated} {}", word("update", "updated")));
    }
    if unchanged > 0 {
        parts.push(format!("{unchanged} unchanged"));
    }
    if unknown > 0 {
        parts.push(format!("{unknown} unknown"));
    }
    if failed > 0 {
        parts.push(format!("{failed} {}", word("fail", "failed")));
    }
    if parts.is_empty() {
        return "nothing to do".to_string();
    }
    parts.join(", ")
}

/// What one entry ended up as, for the summary tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reported {
    Created,
    Updated,
    Unchanged,
    /// The daemon answered OK but did not say what it did: no `X-Station` pair at all, or
    /// one carrying a word this client does not know. NOT `Created` - the write may or
    /// may not have happened and this client cannot tell, so it says so instead of
    /// inventing the most flattering of the two.
    Unknown,
    Failed,
}

/// Read the daemon's `X-Station` pair. THE CLIENT HALF of the contract the daemon's
/// `station_result_pairs` writes, kept pure so both halves are asserted against the same
/// three literals and cannot drift apart unnoticed.
///
/// `None` (the pair absent) and an unrecognised word both mean [`Reported::Unknown`],
/// never `Created`: a client that guesses the flattering answer reports a run of
/// confident creations nothing verified, which is worse than admitting the daemon said
/// something it does not understand.
fn classify_station_outcome(outcome: Option<&str>) -> Reported {
    match outcome {
        Some("created") => Reported::Created,
        Some("updated") => Reported::Updated,
        Some("unchanged") => Reported::Unchanged,
        _ => Reported::Unknown,
    }
}

/// Send one `station add` and print what the daemon says it did. `Ok(None)` means the
/// TRANSPORT died (as distinct from the station being rejected), so the caller stops.
fn send_add(
    conn: &mut MpdConn,
    url: &str,
    name: &str,
    suffix: &str,
) -> Result<Option<Reported>, MpdError> {
    let line = format!("station add {} {}", quote_arg(url), quote_arg(name));
    match conn.command(&line) {
        Ok(pairs) => {
            // NO DEFAULT TO "created". A missing or unrecognised `X-Station` used to be
            // printed as a create and tallied as one, so a daemon too old to carry the
            // pair - or one answering with a word added later - reported a run of
            // confident creations that nothing had verified. The pair IS the contract
            // (`station_result_pairs` in the daemon); when it is absent, the honest
            // report is that we do not know.
            let outcome = pair(&pairs, "X-Station");
            let canonical = pair(&pairs, "Name").unwrap_or_else(|| name.to_string());
            let reported = classify_station_outcome(outcome.as_deref());
            match reported {
                Reported::Unchanged => println!("unchanged  {canonical}{suffix}"),
                Reported::Updated => match pair(&pairs, "X-PrevUrl") {
                    Some(prev) => println!("updated    {canonical}{suffix}  {prev} -> {url}"),
                    None => println!("updated    {canonical}{suffix}"),
                },
                Reported::Created => println!("created    {canonical}{suffix}"),
                Reported::Unknown => match outcome.as_deref() {
                    Some(other) => {
                        println!("unknown    {canonical}{suffix}  (daemon said \"{other}\")")
                    }
                    None => println!("unknown    {canonical}{suffix}  (daemon sent no X-Station)"),
                },
                // Never produced by the classifier; a rejection arrives as an Ack below.
                Reported::Failed => println!("failed     {canonical}{suffix}"),
            }
            Ok(Some(reported))
        }
        // A per-station rejection is recorded and the loop CONTINUES: each entry is an
        // independent idempotent upsert, so a re-run after fixing the cause converges,
        // whereas undoing the ones that landed would destroy real state.
        Err(MpdError::Ack(msg)) => {
            println!("failed     {name}: {msg}");
            Ok(Some(Reported::Failed))
        }
        Err(MpdError::Io(msg)) | Err(MpdError::ConnectionRefused(msg)) => {
            println!("failed     {name}: {msg}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Print the DRY-RUN line for one entry and classify it, sending nothing.
fn report_preview(v: &Verdict, name: &str, url: &str, suffix: &str) -> Reported {
    match v {
        Verdict::Create => {
            println!("would create   {name}{suffix}");
            Reported::Created
        }
        Verdict::Update(prev) => {
            match prev {
                Some(p) => println!("would update   {name}{suffix}  {p} -> {url}"),
                None => println!("would update   {name}{suffix}"),
            }
            Reported::Updated
        }
        Verdict::Unchanged => {
            println!("unchanged      {name}{suffix}");
            Reported::Unchanged
        }
        Verdict::Conflict => {
            println!("would fail     {name}: name already used by another station");
            Reported::Failed
        }
    }
}

/// The saved stations as (name, url), read from the daemon's `Stations` browse dir.
/// That listing emits `file:` = the raw stream url plus `Title:`/`Name:` = the label,
/// which is exactly what a preview needs and needs no new daemon surface.
fn saved_stations(conn: &mut MpdConn) -> Result<Vec<(String, String)>, MpdError> {
    let pairs = conn.command("lsinfo \"Stations\"")?;
    Ok(parse_saved_stations(&pairs))
}

/// Bucket `lsinfo Stations` pairs into (name, url). A new `file:` opens a row; the
/// `Name:` that follows labels it. A row whose name never arrived falls back to its url,
/// which is what the daemon's own derived-label path would have produced anyway.
pub fn parse_saved_stations(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "file" => out.push((v.clone(), v.clone())),
            "Name" => {
                if let Some(last) = out.last_mut() {
                    last.0 = v.clone();
                }
            }
            _ => {}
        }
    }
    out
}

/// The CLIENT-side preview of the daemon's upsert rule, for `--dry-run` only.
///
/// It MIRRORS `station_upsert` in the daemon deliberately rather than sharing code: the
/// daemon owns the decision on every real write and re-computes it there, so a drift
/// here can only ever make a PREVIEW slightly wrong, never a write wrong. Name equality
/// is ASCII-case-folded, the same rule `add station/<name>` resolves with.
pub fn preview_verdict(saved: &[(String, String)], url: &str, name: &str) -> Verdict {
    let by_url = saved.iter().position(|(_, u)| u == url);
    let by_name = saved.iter().position(|(n, _)| n.eq_ignore_ascii_case(name));
    match (by_url, by_name) {
        (Some(u), Some(n)) if u == n => {
            if saved[u].0 == name {
                Verdict::Unchanged
            } else {
                Verdict::Update(None)
            }
        }
        (Some(_), Some(_)) => Verdict::Conflict,
        (None, Some(n)) => Verdict::Update(Some(saved[n].1.clone())),
        (Some(_), None) => Verdict::Update(None),
        (None, None) => Verdict::Create,
    }
}

/// Why an entry cannot be saved next to the entries ALREADY claimed by this same run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clash {
    /// Neither key is spoken for: save it.
    None,
    /// An earlier entry carries this EXACT (url, name) pair - the same station listed
    /// twice. Nothing is at stake, so it is skipped rather than failed.
    Duplicate,
    /// An earlier entry of this run already claimed this NAME, under the url carried here.
    Name(String),
    /// An earlier entry of this run already claimed this URL, under the name carried here.
    Url(String),
}

/// Does saving `(url, name)` fight with something this run already saved?
///
/// The daemon decides ONE `station add` at a time, against the stations that exist at
/// that instant - it cannot see that the row it is about to rename is a different stream
/// from the same import. So the run has to keep both of its idempotency keys unique: a
/// second entry that shares exactly one of them describes a DIFFERENT station claiming a
/// taken key, and sending it would rewrite the row the previous entry just saved. Worse,
/// it never settles: the next run rewrites it back, so the import writes to the server
/// forever and keeps only whichever entry went last. Two mirror urls under one file stem
/// (a `.pls` with no `TitleN`) and two files sharing a `Title1` are both this shape.
///
/// Name equality is ASCII-case-folded, the SAME rule the daemon resolves a name with;
/// url equality is exact, because the url is the station's identity byte for byte. Only a
/// byte-identical pair is a harmless [`Clash::Duplicate`] - a matching url under a
/// differently-CASED name is still a rewrite of the saved label.
pub fn run_clash(claimed: &[(String, String)], url: &str, name: &str) -> Clash {
    if claimed.iter().any(|(n, u)| u == url && n == name) {
        return Clash::Duplicate;
    }
    if let Some((_, u)) = claimed.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Clash::Name(u.clone());
    }
    if let Some((n, _)) = claimed.iter().find(|(_, u)| u == url) {
        return Clash::Url(n.clone());
    }
    Clash::None
}

/// Every `.pls` file named by `paths`: a directory contributes its own `.pls` files
/// (NOT recursive - a stream collection is a flat drawer, and recursing would silently
/// pull in whatever else lives below), sorted by file name so two runs print in the same
/// order. A named file is taken as-is whatever its extension, because naming it is an
/// explicit choice.
fn collect_pls_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for p in paths {
        let meta = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
        if meta.is_dir() {
            let mut here: Vec<PathBuf> = std::fs::read_dir(p)
                .map_err(|e| format!("{}: {e}", p.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|f| f.is_file() && has_pls_extension(f))
                .collect();
            here.sort();
            out.extend(here);
        } else {
            out.push(p.clone());
        }
    }
    Ok(out)
}

/// Is this a `.pls` file? Extension matched ASCII-case-insensitively (`.PLS` happens).
fn has_pls_extension(p: &Path) -> bool {
    p.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pls"))
}

/// Expand a leading `~` to `$HOME`. The shell normally does this, but a quoted path
/// (`dj stations import "~/radio streams"`) reaches us literally, and silently failing
/// to find a directory the user can see is the worst outcome of the two.
fn shell_expand_home(s: &str) -> String {
    expand_home_with(s, std::env::var("HOME").ok().as_deref())
}

/// The pure half of [`shell_expand_home`], with `$HOME` threaded in so it is testable
/// without mutating the process environment. A tilde anywhere but the front is an
/// ordinary character in an ordinary path and is left alone.
fn expand_home_with(s: &str, home: Option<&str>) -> String {
    let Some(home) = home else { return s.to_string() };
    if s == "~" {
        return home.to_string();
    }
    match s.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None => s.to_string(),
    }
}

fn pair(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {

    // Deleting from the user's own Navidrome is destructive and not undoable from the
    // CLI, so `rm` must ask - the bar `dj clear` already sets for the far more
    // recoverable queue. Pinned at the source level because the confirm reads stdin.
    #[test]
    fn stations_rm_asks_before_deleting() {
        let src = include_str!("stations.rs");
        let body = src
            .split("fn rm(conn: &mut MpdConn")
            .nth(1)
            .expect("rm exists");
        let head = &body[..body.find("match conn.command").expect("rm sends a command")];
        assert!(
            head.contains("confirm("),
            "rm must confirm BEFORE it sends the delete, not after: {head}"
        );
    }
    use super::*;

    #[test]
    fn parses_the_stations_browse_listing() {
        let pairs = vec![
            ("file".to_string(), "https://n/1".to_string()),
            ("Title".to_string(), "NTS 1".to_string()),
            ("Name".to_string(), "NTS 1".to_string()),
            ("file".to_string(), "http://k/live".to_string()),
            ("Title".to_string(), "KFJC 89.7 FM".to_string()),
            ("Name".to_string(), "KFJC 89.7 FM".to_string()),
        ];
        assert_eq!(
            parse_saved_stations(&pairs),
            vec![
                ("NTS 1".to_string(), "https://n/1".to_string()),
                ("KFJC 89.7 FM".to_string(), "http://k/live".to_string()),
            ]
        );
        assert!(parse_saved_stations(&[]).is_empty());
    }

    #[test]
    fn an_unnamed_row_falls_back_to_its_url() {
        let pairs = vec![("file".to_string(), "https://n/1".to_string())];
        assert_eq!(
            parse_saved_stations(&pairs),
            vec![("https://n/1".to_string(), "https://n/1".to_string())]
        );
    }

    #[test]
    fn preview_verdict_mirrors_the_daemon_rule() {
        let saved = vec![
            ("NTS 4 To The Floor".to_string(), "https://n/mixtape5".to_string()),
            ("KFJC 89.7 FM".to_string(), "http://k/live".to_string()),
        ];
        assert_eq!(preview_verdict(&saved, "https://new/", "New One"), Verdict::Create);
        assert_eq!(
            preview_verdict(&saved, "https://n/mixtape5", "NTS 4 To The Floor"),
            Verdict::Unchanged
        );
        // Renamed in the .pls: same url, new label.
        assert_eq!(preview_verdict(&saved, "https://n/mixtape5", "NTS FTF"), Verdict::Update(None));
        // The endpoint moved: same name, new url - the old url is echoed.
        assert_eq!(
            preview_verdict(&saved, "http://k/live-320", "kfjc 89.7 fm"),
            Verdict::Update(Some("http://k/live".to_string()))
        );
        // The name is spoken for by a DIFFERENT station.
        assert_eq!(
            preview_verdict(&saved, "https://n/mixtape5", "KFJC 89.7 FM"),
            Verdict::Conflict
        );
    }

    #[test]
    fn preview_of_a_second_run_is_all_unchanged() {
        // The idempotence promise, previewed: after one import, every entry reads back
        // as Unchanged, so `--dry-run` re-run is itself the verification gesture.
        let pls = vec![
            ("NTS 4 To The Floor", "https://stream-mixtape-geo.ntslive.net/mixtape5"),
            ("Moon Mission Recordings, Tokyo Deep and Electronic", "http://uk5.internet-radio.com:8306/stream"),
            ("Oxigenio 102.6 FM Lisboa", "http://proic1.evspt.com:80/oxigenio_aac"),
        ];
        let saved: Vec<(String, String)> =
            pls.iter().map(|(n, u)| (n.to_string(), u.to_string())).collect();
        for (name, url) in &pls {
            assert_eq!(preview_verdict(&saved, url, name), Verdict::Unchanged, "{name}");
        }
    }

    #[test]
    fn the_summary_states_only_what_happened() {
        assert_eq!(summary(false, 3, 1, 18, 0, 1), "3 created, 1 updated, 18 unchanged, 1 failed");
        // The second import of the same collection: no write verbs at all.
        assert_eq!(summary(false, 0, 0, 22, 0, 0), "22 unchanged");
        assert_eq!(summary(true, 22, 0, 0, 0, 0), "22 would create");
        assert_eq!(summary(true, 0, 0, 0, 0, 1), "1 would fail");
        assert_eq!(summary(false, 0, 0, 0, 0, 0), "nothing to do");
        // An unconfirmable outcome gets its own word in the tally rather than hiding
        // inside "created" - the run is not a clean import and must not read like one.
        assert_eq!(summary(false, 1, 0, 0, 2, 0), "1 created, 2 unknown");
    }

    // ── the X-Station contract, CLIENT half (task 4i3s3ry) ──────────────────
    //
    // The daemon's `station_result_pairs` writes exactly three words. This asserts the
    // client reads exactly those three and, crucially, refuses to GUESS for anything
    // else: the old code defaulted a missing pair to "created" and tallied a create, so
    // an older daemon (or a word added later) produced a run of confident creations that
    // nothing had verified.
    #[test]
    fn the_three_contract_words_classify_and_nothing_else_does() {
        assert_eq!(classify_station_outcome(Some("created")), Reported::Created);
        assert_eq!(classify_station_outcome(Some("updated")), Reported::Updated);
        assert_eq!(classify_station_outcome(Some("unchanged")), Reported::Unchanged);
    }

    #[test]
    fn a_missing_or_unrecognised_x_station_is_unknown_never_created() {
        assert_eq!(
            classify_station_outcome(None),
            Reported::Unknown,
            "no X-Station pair at all must never be reported as a create"
        );
        assert_eq!(
            classify_station_outcome(Some("relocated")),
            Reported::Unknown,
            "a word this client does not know must never be reported as a create"
        );
        // Case matters: the daemon emits lowercase literals, and silently accepting a
        // near-miss would let a real contract break through as a success.
        assert_eq!(classify_station_outcome(Some("Created")), Reported::Unknown);
        assert_eq!(classify_station_outcome(Some("")), Reported::Unknown);
    }

    #[test]
    fn pls_extension_matching_is_case_insensitive() {
        assert!(has_pls_extension(Path::new("/a/nts_1.pls")));
        assert!(has_pls_extension(Path::new("/a/NTS_1.PLS")));
        assert!(!has_pls_extension(Path::new("/a/heard music.txt")));
        assert!(!has_pls_extension(Path::new("/a/nopls")));
    }

    #[test]
    fn two_entries_of_one_run_may_not_fight_over_a_key() {
        // The daemon decides ONE call at a time: it cannot see that the row it is about
        // to rename belongs to a different stream of the same import. So the run itself
        // has to keep both keys unique.
        let claimed = vec![
            ("Some Station".to_string(), "http://mirror1/".to_string()),
            ("KFJC 89.7 FM".to_string(), "http://k/live".to_string()),
        ];
        assert_eq!(run_clash(&claimed, "http://new/", "New One"), Clash::None);
        // The same station listed twice: nothing to fight over.
        assert_eq!(run_clash(&claimed, "http://mirror1/", "Some Station"), Clash::Duplicate);
        // A second mirror url under the file-stem name - the multi-entry .pls shape.
        assert_eq!(
            run_clash(&claimed, "http://mirror2/", "Some Station"),
            Clash::Name("http://mirror1/".to_string())
        );
        // Case-folded, because that is the rule the daemon resolves a name with. Same
        // url plus a differently-CASED name is a rewrite of the label, not a duplicate.
        assert_eq!(
            run_clash(&claimed, "http://mirror9/", "some station"),
            Clash::Name("http://mirror1/".to_string())
        );
        assert_eq!(
            run_clash(&claimed, "http://mirror1/", "SOME STATION"),
            Clash::Name("http://mirror1/".to_string())
        );
        // One stream listed under two names would flip-flop on the url key instead.
        assert_eq!(
            run_clash(&claimed, "http://k/live", "KFJC"),
            Clash::Url("KFJC 89.7 FM".to_string())
        );
        assert_eq!(run_clash(&[], "http://a/", "A"), Clash::None);
    }

    #[test]
    fn tilde_expands_only_at_the_front() {
        let home = Some("/home/tester");
        assert_eq!(expand_home_with("~/radio-streams", home), "/home/tester/radio-streams");
        assert_eq!(expand_home_with("~", home), "/home/tester");
        // A tilde anywhere else is a real character in a real path.
        assert_eq!(expand_home_with("/tmp/a~b", home), "/tmp/a~b");
        assert_eq!(expand_home_with("./rel", home), "./rel");
        // No HOME at all: the path is used verbatim rather than mangled.
        assert_eq!(expand_home_with("~/x", None), "~/x");
    }
}

/// The import loop driven END TO END over a socket, against a fake daemon that mirrors
/// `station_upsert` exactly.
///
/// The unit tests above cover the pure rules; only a real run of [`import`] can prove the
/// promise the gesture actually makes - that a SECOND import over the same files writes
/// nothing. Convergence is a property of the whole loop (parse, name, clash, send), so it
/// is asserted where it lives: `writes` on the fake server, before and after a re-run.
#[cfg(test)]
mod import_over_a_socket {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// The fake server's whole state: the saved stations plus a count of every write it
    /// was actually asked to perform. `writes` is the convergence probe - a second import
    /// that adds even one is an import that never settles.
    #[derive(Default)]
    struct Saved {
        rows: Vec<Row>,
        writes: usize,
    }

    struct Row {
        url: String,
        name: String,
    }

    /// `station add` decided by the SAME rule as `station_upsert` in the daemon: url is
    /// identity, name is the label, name equality is ASCII-case-folded, and a name held
    /// by a different row is refused.
    fn station_add(saved: &mut Saved, url: &str, name: &str) -> String {
        let by_url = saved.rows.iter().position(|r| r.url == url);
        let by_name = saved.rows.iter().position(|r| r.name.eq_ignore_ascii_case(name));
        let target = match (by_url, by_name) {
            (Some(u), Some(n)) if u == n => {
                if saved.rows[u].name == name {
                    return format!("X-Station: unchanged\nName: {name}\nfile: {url}\nOK\n");
                }
                Some(u)
            }
            (Some(_), Some(_)) => {
                return "ACK [56@0] {station} name already used by another station\n".to_string()
            }
            (Some(u), None) => Some(u),
            (None, Some(n)) => Some(n),
            (None, None) => None,
        };
        saved.writes += 1;
        match target {
            Some(i) => {
                let prev = saved.rows[i].url.clone();
                saved.rows[i].url = url.to_string();
                saved.rows[i].name = name.to_string();
                let mut r = format!("X-Station: updated\nName: {name}\nfile: {url}\n");
                if prev != url {
                    r.push_str(&format!("X-PrevUrl: {prev}\n"));
                }
                r.push_str("OK\n");
                r
            }
            None => {
                saved.rows.push(Row { url: url.to_string(), name: name.to_string() });
                format!("X-Station: created\nName: {name}\nfile: {url}\nOK\n")
            }
        }
    }

    fn respond(args: &[String], state: &Mutex<Saved>) -> String {
        match args.first().map(|s| s.as_str()) {
            Some("lsinfo") => {
                let saved = state.lock().unwrap();
                let mut out = String::new();
                for r in &saved.rows {
                    out.push_str(&format!(
                        "file: {}\nTitle: {}\nName: {}\n",
                        r.url, r.name, r.name
                    ));
                }
                out.push_str("OK\n");
                out
            }
            Some("station") if args.get(1).map(|s| s.as_str()) == Some("add") => {
                let (url, name) = (args[2].clone(), args[3].clone());
                station_add(&mut state.lock().unwrap(), &url, &name)
            }
            _ => "ACK [5@0] {} unknown command\n".to_string(),
        }
    }

    /// The daemon's own tokenizer shape: bare words, or a double-quoted argument in which
    /// `\"` and `\\` are escapes - which is exactly what `quote_arg` emits.
    fn split_args(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut escaped = false;
        let mut started = false;
        for c in line.chars() {
            if escaped {
                cur.push(c);
                escaped = false;
                continue;
            }
            match c {
                '\\' if in_quotes => escaped = true,
                '"' => {
                    in_quotes = !in_quotes;
                    started = true;
                }
                ' ' if !in_quotes => {
                    if started || !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                _ => cur.push(c),
            }
        }
        if started || !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// Serve exactly `conns` connections and return, so the test JOINS the thread rather
    /// than leaking a listener for the rest of the test binary's life.
    fn serve(listener: TcpListener, conns: usize, state: Arc<Mutex<Saved>>) {
        for _ in 0..conns {
            let Ok((stream, _)) = listener.accept() else { return };
            let mut w = stream.try_clone().expect("clone");
            if w.write_all(b"OK MPD 0.24.0\n").is_err() {
                continue;
            }
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let reply = respond(&split_args(&line), &state);
                if w.write_all(reply.as_bytes()).is_err() {
                    break;
                }
            }
        }
    }

    /// A fake daemon plus a scratch directory of `.pls` files, torn down together.
    struct Harness {
        dir: PathBuf,
        port: u16,
        state: Arc<Mutex<Saved>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        /// `conns` is how many `import` runs the test will make - one connection each.
        fn new(tag: &str, conns: usize, files: &[(&str, &str)]) -> Harness {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "hypodj-stations-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            for (name, text) in files {
                std::fs::write(dir.join(name), text).expect("write .pls");
            }
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().expect("addr").port();
            let state = Arc::new(Mutex::new(Saved::default()));
            let thread = std::thread::spawn({
                let state = Arc::clone(&state);
                move || serve(listener, conns, state)
            });
            Harness { dir, port, state, thread: Some(thread) }
        }

        /// One `dj stations import <dir>` run: connect, import, hang up.
        fn import_run(&self) -> bool {
            let mut conn = MpdConn::connect("127.0.0.1", self.port).expect("connect");
            let arg = vec![self.dir.to_string_lossy().to_string()];
            import(&mut conn, &arg).expect("import")
        }

        fn writes(&self) -> usize {
            self.state.lock().unwrap().writes
        }

        /// The saved set as (name, url), in save order.
        fn rows(&self) -> Vec<(String, String)> {
            self.state
                .lock()
                .unwrap()
                .rows
                .iter()
                .map(|r| (r.name.clone(), r.url.clone()))
                .collect()
        }

        fn finish(mut self) {
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_multi_entry_pls_saves_one_station_and_never_flip_flops() {
        // The ordinary shape a station website hands out: three mirror urls, no TitleN,
        // so every entry would be named after the file. Sending all three would make the
        // daemon rename ONE row three times, and the next run would rename it back
        // forever - an import that never converges and keeps only the last mirror.
        let pls = "[playlist]\nNumberOfEntries=3\n\
                   File1=http://mirror1.example/stream\nLength1=-1\n\
                   File2=http://mirror2.example/stream\nLength2=-1\n\
                   File3=http://mirror3.example/stream\nLength3=-1\nVersion=2";
        let h = Harness::new("mirrors", 2, &[("Some Station.pls", pls)]);

        // Run 1: the first mirror lands, the other two are REFUSED and reported, so the
        // run exits non-zero rather than silently dropping two streams.
        assert!(!h.import_run(), "a refused entry must make the run fail loudly");
        assert_eq!(h.writes(), 1, "exactly one write on the first import");
        assert_eq!(
            h.rows(),
            vec![("Some Station".to_string(), "http://mirror1.example/stream".to_string())]
        );

        // Run 2: the promise. Zero writes, same row, same url - converged.
        assert!(!h.import_run());
        assert_eq!(h.writes(), 1, "the second import must write NOTHING");
        assert_eq!(
            h.rows(),
            vec![("Some Station".to_string(), "http://mirror1.example/stream".to_string())]
        );
        h.finish();
    }

    #[test]
    fn two_files_sharing_a_title_do_not_rewrite_each_other() {
        // Same fight, across files: two `.pls` whose Title1 is equal are two streams
        // claiming one label, and the label is how the daemon resolves a station.
        let a = "[playlist]\nNumberOfEntries=1\nFile1=http://a.example/1\nTitle1=Duplicated Name\nVersion=2";
        let b = "[playlist]\nNumberOfEntries=1\nFile1=http://b.example/2\nTitle1=Duplicated Name\nVersion=2";
        let h = Harness::new("dupnames", 2, &[("a.pls", a), ("b.pls", b)]);

        assert!(!h.import_run());
        assert_eq!(h.writes(), 1);
        assert_eq!(
            h.rows(),
            vec![("Duplicated Name".to_string(), "http://a.example/1".to_string())]
        );

        assert!(!h.import_run());
        assert_eq!(h.writes(), 1, "the second import must write NOTHING");
        assert_eq!(
            h.rows(),
            vec![("Duplicated Name".to_string(), "http://a.example/1".to_string())]
        );
        h.finish();
    }

    #[test]
    fn one_stream_listed_under_two_names_keeps_the_first_name() {
        // The other key. Two entries pointing at ONE url under two labels would rename
        // the same row back and forth, because the url is the station's identity.
        let pls = "[playlist]\nNumberOfEntries=2\n\
                   File1=http://one.example/stream\nTitle1=First Name\n\
                   File2=http://one.example/stream\nTitle2=Second Name\nVersion=2";
        let h = Harness::new("twonames", 2, &[("dup.pls", pls)]);

        assert!(!h.import_run());
        assert_eq!(h.writes(), 1);
        assert_eq!(
            h.rows(),
            vec![("First Name".to_string(), "http://one.example/stream".to_string())]
        );

        assert!(!h.import_run());
        assert_eq!(h.writes(), 1, "the second import must write NOTHING");
        h.finish();
    }

    #[test]
    fn a_genuinely_multi_station_pls_still_imports_every_entry() {
        // The guard must not overcorrect: distinct urls under distinct titles are simply
        // several stations, and all of them must land - then stay put on a re-run.
        let pls = "[playlist]\nNumberOfEntries=2\n\
                   File1=http://uk5.internet-radio.com:8306/stream\nTitle1=Moon Mission Recordings\n\
                   File2=http://proic1.evspt.com:80/oxigenio_aac\nTitle2=Oxigenio 102.6 FM Lisboa\nVersion=2";
        let h = Harness::new("multi", 2, &[("collection.pls", pls)]);

        assert!(h.import_run(), "nothing here clashes, so the run succeeds");
        assert_eq!(h.writes(), 2);
        assert_eq!(
            h.rows(),
            vec![
                (
                    "Moon Mission Recordings".to_string(),
                    "http://uk5.internet-radio.com:8306/stream".to_string()
                ),
                (
                    "Oxigenio 102.6 FM Lisboa".to_string(),
                    "http://proic1.evspt.com:80/oxigenio_aac".to_string()
                ),
            ]
        );

        assert!(h.import_run());
        assert_eq!(h.writes(), 2, "the second import must write NOTHING");
        h.finish();
    }

    #[test]
    fn the_same_entry_listed_twice_is_a_no_op_not_a_failure() {
        // A byte-identical repeat is the one collision with nothing at stake: the second
        // write would be an `unchanged`, so it is skipped and the run still succeeds.
        let pls = "[playlist]\nNumberOfEntries=2\n\
                   File1=http://one.example/stream\nTitle1=Same Thing\n\
                   File2=http://one.example/stream\nTitle2=Same Thing\nVersion=2";
        let h = Harness::new("identical", 2, &[("same.pls", pls)]);

        assert!(h.import_run(), "a duplicate line is not a failure");
        assert_eq!(h.writes(), 1);
        assert!(h.import_run());
        assert_eq!(h.writes(), 1);
        assert_eq!(
            h.rows(),
            vec![("Same Thing".to_string(), "http://one.example/stream".to_string())]
        );
        h.finish();
    }

    #[test]
    fn a_dry_run_previews_the_clash_and_writes_nothing() {
        // `--dry-run` is the advertised verification gesture, so it must SEE the clash it
        // is previewing rather than printing three creates for one name.
        let pls = "[playlist]\nNumberOfEntries=2\n\
                   File1=http://mirror1.example/stream\n\
                   File2=http://mirror2.example/stream\nVersion=2";
        let h = Harness::new("dryrun", 1, &[("Some Station.pls", pls)]);
        let mut conn = MpdConn::connect("127.0.0.1", h.port).expect("connect");
        let args =
            vec![h.dir.to_string_lossy().to_string(), "--dry-run".to_string()];
        assert!(!import(&mut conn, &args).expect("import"));
        assert_eq!(h.writes(), 0, "a dry run sends no writes at all");
        assert!(h.rows().is_empty());
        drop(conn);
        h.finish();
    }

    #[test]
    fn tokenizer_round_trips_what_quote_arg_emits() {
        // The fake daemon is only evidence if it reads the same bytes the real one does.
        let line = format!(
            "station add {} {}",
            quote_arg("http://a/?v=1&x=2"),
            quote_arg("A \"quoted\" name")
        );
        assert_eq!(
            split_args(&line),
            vec!["station", "add", "http://a/?v=1&x=2", "A \"quoted\" name"]
        );
        assert_eq!(split_args("lsinfo \"Stations\""), vec!["lsinfo", "Stations"]);
    }
}
