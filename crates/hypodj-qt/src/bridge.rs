//! The one QObject QML talks to. It owns the request sender into the shared worker
//! (`hypodj_app::worker`, the same sockets and routing djg uses), folds each
//! [`Msg`] into JSON properties on the GUI thread, and decides nothing of its own:
//! every word comes from `hypodj_app::view` or hypodj-client.

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    unsafe extern "C++Qt" {
        include!(<QtQuick/QQuickWindow>);
        #[qobject]
        type QQuickWindow;
    }

    unsafe extern "C++" {
        include!("hypodj-qt/cpp/shim.h");
        fn djq_setup_app();
        unsafe fn djq_grab(w: *mut QQuickWindow, path: &QString) -> bool;
    }

    unsafe extern "RustQt" {
        #[qobject]
        #[qml_element]
        /// The Playing page as JSON: words, transport, volume, Up Next, status.
        #[qproperty(QString, now)]
        /// The Albums page as JSON: `{rows, note}`.
        #[qproperty(QString, albums)]
        /// The opened album as JSON: `{uri, title, rows}`.
        #[qproperty(QString, album)]
        /// The search page as JSON: `{query, error, hits}`.
        #[qproperty(QString, search)]
        /// The cover as a `data:` URL, or empty.
        #[qproperty(QString, cover)]
        /// The instrument's colours as JSON, or empty under high contrast (the room).
        #[qproperty(QString, instrument)]
        /// HypoDJ's last reply; QML shows it in the reply card above the ask entry.
        #[qproperty(QString, reply)]
        type Backend = super::BackendRust;

        /// A plan echoed and waiting: QML asks, then calls `confirm` or `cancel`.
        #[qsignal]
        #[cxx_name = "confirmPlan"]
        fn confirm_plan(self: Pin<&mut Backend>, token: QString, body: QString);
        /// A destructive verb the router gates behind a yes/no (`clear`).
        #[qsignal]
        #[cxx_name = "confirmCommand"]
        fn confirm_command(self: Pin<&mut Backend>, command: QString, question: QString);
        #[qsignal]
        #[cxx_name = "showPlaying"]
        fn show_playing(self: Pin<&mut Backend>);
        /// A new reply landed (the same text can arrive twice; the card reopens).
        #[qsignal]
        fn said(self: Pin<&mut Backend>);
        #[qsignal]
        #[cxx_name = "albumOpened"]
        fn album_opened(self: Pin<&mut Backend>);

        #[qinvokable]
        unsafe fn start(self: Pin<&mut Backend>, window: *mut QQuickWindow);
        /// One MPD command line (transport, setvol, play N).
        #[qinvokable]
        fn command(self: &Backend, line: &QString);
        #[qinvokable]
        #[cxx_name = "playUri"]
        fn play_uri(self: &Backend, uri: &QString);
        /// The ask entry, routed exactly like `djc`.
        #[qinvokable]
        fn ask(self: &Backend, text: &QString);
        #[qinvokable]
        fn confirm(self: &Backend, token: &QString);
        #[qinvokable]
        fn cancel(self: &Backend, token: &QString);
        #[qinvokable]
        #[cxx_name = "openAlbum"]
        fn open_album(self: &Backend, uri: &QString, title: &QString);
        #[qinvokable]
        #[cxx_name = "searchFor"]
        fn search_for(self: Pin<&mut Backend>, query: &QString);
        /// The host room changed (palette lightness, contrast): repaint the instrument.
        #[qinvokable]
        #[cxx_name = "setRoom"]
        fn set_room(self: Pin<&mut Backend>, dark: bool, high_contrast: bool);
        /// The headless proof hook's inputs (`DJQ_*`), read by QML.
        #[qinvokable]
        fn env(self: &Backend, name: &QString) -> QString;
        #[qinvokable]
        fn screenshot(self: &Backend, path: &QString) -> bool;
    }

    impl cxx_qt::Threading for Backend {}
}

use std::pin::Pin;
use std::sync::mpsc::Sender;

use cxx_qt::{CxxQtType, Threading};
use cxx_qt_lib::{QByteArray, QByteArrayBase64Options, QImage, QString};
use serde_json::{json, Value};

use hypodj_app::view::{self, HitKind};
use hypodj_app::worker::{self, Msg, Req};
use hypodj_client::config::{self, Env};
use hypodj_client::model::{NowPlaying, QueueItem};

use qobject::*;

#[derive(Default)]
pub struct BackendRust {
    now: QString,
    albums: QString,
    album: QString,
    search: QString,
    cover: QString,
    instrument: QString,
    reply: QString,
    tx: Option<Sender<Req>>,
    /// The QQuickWindow, for the screenshot hook only (address; GUI thread only).
    window: usize,
    art_file: Option<String>,
    cover_avg: Option<[u8; 3]>,
    dark: bool,
    high_contrast: bool,
    search_query: String,
}

fn join(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("{a} \u{b7} {b}")),
        (Some(x), None) | (None, Some(x)) => Some(x.to_string()),
        (None, None) => None,
    }
}

fn q(s: &str) -> QString {
    QString::from(s)
}

/// The Playing page in words: the same fold as djg's `App::now_playing`.
pub fn now_json(now: &NowPlaying, queue: &[QueueItem]) -> Value {
    let st = view::status(now.server.as_deref(), now.store.as_deref());
    let banner = st.problem.then(|| {
        format!(
            "{} - songs on disk still play",
            hypodj_client::model::server_words(now.server.as_deref().unwrap_or("")).0
        )
    });
    let nothing = now.title.is_none() && now.name.is_none() && now.file.is_none();
    let title = if nothing {
        "Nothing playing"
    } else {
        now.title.as_deref().or(now.name.as_deref()).unwrap_or("Unknown")
    };
    let album = now
        .album
        .as_deref()
        .unwrap_or(if nothing { "Ask for something below, or pick an album." } else { "" });
    // Ember marks only what HypoDJ is holding for you: kept, or armed.
    let mut held: Vec<String> = Vec::new();
    if now.starred {
        held.push("kept".into());
    }
    for s in hypodj_client::spoken::armed_hold(&now.armed) {
        held.push(s.text());
    }
    let playing = now.state.as_deref() == Some("play");
    // Resume is `pause 0` (startle-safe, from the playhead), never `play`, which
    // restarts the track.
    let verb = match now.state.as_deref() {
        Some("play") => "pause 1",
        Some("pause") => "pause 0",
        _ => "play",
    };
    let start = now.song.map(|s| s + 1).unwrap_or(0);
    let up_next: Vec<Value> = queue
        .iter()
        .skip(start)
        .take(8)
        .map(|it| {
            json!({
                "title": it.title,
                "sub": join(it.artist.as_deref(), view::queue_availability(it)).unwrap_or_default(),
                "pos": it.pos,
                "onDisk": view::queue_on_disk(it),
            })
        })
        .collect();
    json!({
        "title": title,
        "artist": now.artist.clone().unwrap_or_default(),
        "album": album,
        "held": held.join(" \u{b7} "),
        "playing": playing,
        "verb": verb,
        "volume": now.volume.filter(|v| *v >= 0).unwrap_or(-1),
        "upNext": up_next,
        "status": { "label": st.label, "problem": st.problem, "details": st.details.join("\n"), "icon": st.icon },
        "banner": banner.unwrap_or_default(),
        // The server is down: Rows keeps only on-disk rows enabled.
        "down": view::server_down(now.server.as_deref()),
    })
}

/// The cover's average colour on a coarse grid, and the cover as a `data:` URL.
fn cover_of(bytes: &[u8]) -> Option<([u8; 3], QString)> {
    let img = QImage::from_data(bytes, None)?;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let step = (w.max(h) / 32).max(1);
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for y in (0..h).step_by(step as usize) {
        for x in (0..w).step_by(step as usize) {
            let c = img.pixel_color(x, y);
            r += c.red() as u64;
            g += c.green() as u64;
            b += c.blue() as u64;
            n += 1;
        }
    }
    let n = n.max(1);
    let b64 = QByteArray::from(bytes).to_base64(QByteArrayBase64Options::default());
    let url = format!("data:image/*;base64,{}", String::from_utf8_lossy(b64.as_slice()));
    Some(([(r / n) as u8, (g / n) as u8, (b / n) as u8], QString::from(&url)))
}

impl qobject::Backend {
    fn send(&self, r: Req) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(r);
        }
    }

    /// Start the worker and the pump that brings its messages to the GUI thread.
    unsafe fn start(mut self: Pin<&mut Self>, window: *mut QQuickWindow) {
        if self.tx.is_some() {
            return;
        }
        let env = Env { get: &|k| std::env::var(k).ok() };
        let (host, port) = config::resolve(None, None, &env);
        let (out_tx, out_rx) = async_channel::unbounded::<Msg>();
        let handles = worker::spawn(host, port, out_tx);
        {
            let mut me = self.as_mut().rust_mut();
            me.tx = Some(handles.cmd);
            me.window = window as usize;
        }
        let qt = self.qt_thread();
        std::thread::spawn(move || {
            while let Ok(msg) = out_rx.recv_blocking() {
                // Err after teardown: the window is gone, nothing to fold into.
                if qt.queue(move |b| b.apply(msg)).is_err() {
                    return;
                }
            }
        });
        self.restyle();
    }

    fn command(&self, line: &QString) {
        self.send(Req::Command(line.to_string()));
    }

    fn play_uri(&self, uri: &QString) {
        self.send(Req::PlayUri(uri.to_string()));
    }

    fn ask(&self, text: &QString) {
        let t = text.to_string().trim().to_string();
        if !t.is_empty() {
            self.send(Req::Ask(t));
        }
    }

    fn confirm(&self, token: &QString) {
        self.send(Req::Confirm(token.to_string()));
    }

    fn cancel(&self, token: &QString) {
        self.send(Req::Cancel(token.to_string()));
    }

    fn open_album(&self, uri: &QString, title: &QString) {
        self.send(Req::Album { uri: uri.to_string(), title: title.to_string() });
    }

    fn search_for(mut self: Pin<&mut Self>, query: &QString) {
        let qy = query.to_string().trim().to_string();
        self.as_mut().rust_mut().search_query = qy.clone();
        if !qy.is_empty() {
            self.send(Req::Search(qy));
        }
    }

    fn set_room(mut self: Pin<&mut Self>, dark: bool, high_contrast: bool) {
        {
            let mut me = self.as_mut().rust_mut();
            me.dark = dark;
            me.high_contrast = high_contrast;
        }
        self.restyle();
    }

    fn env(&self, name: &QString) -> QString {
        let n = name.to_string();
        if !n.starts_with("DJQ_") {
            return QString::default();
        }
        std::env::var(n).map(|v| q(&v)).unwrap_or_default()
    }

    fn screenshot(&self, path: &QString) -> bool {
        unsafe { djq_grab(self.window as *mut QQuickWindow, path) }
    }

    fn restyle(self: Pin<&mut Self>) {
        let inst = hypodj_style::qt::instrument(self.cover_avg, self.dark, self.high_contrast)
            .map(|i| json!({"ground": i.ground, "ink": i.ink, "quiet": i.ink_quiet, "ember": i.ember}).to_string())
            .unwrap_or_default();
        self.set_instrument(q(&inst));
    }

    fn say(mut self: Pin<&mut Self>, text: &str) {
        self.as_mut().set_reply(q(text));
        self.said();
    }

    fn apply(mut self: Pin<&mut Self>, msg: Msg) {
        match msg {
            Msg::Snapshot { now, queue } => {
                self.as_mut().set_now(q(&now_json(&now, &queue).to_string()));
                // Cover: ask once per track.
                if self.art_file != now.file {
                    {
                        let mut me = self.as_mut().rust_mut();
                        me.art_file = now.file.clone();
                        me.cover_avg = None;
                    }
                    self.as_mut().set_cover(QString::default());
                    self.as_mut().restyle();
                    if let Some(f) = now.file.clone() {
                        if f.starts_with("song/") || now.cover.is_some() {
                            self.send(Req::Art(f));
                        }
                    }
                }
            }
            Msg::Down(why) => {
                eprintln!("djq: down - {why}");
                let v = json!({
                    "title": "Nothing playing", "artist": "", "album": "", "held": "",
                    "playing": false, "verb": "play", "volume": -1, "upNext": [],
                    "status": {"label": "Not connected", "problem": true, "details": why, "icon": "network-offline-symbolic"},
                    "banner": why,
                });
                // Keep the last state on screen: only the status and banner change.
                let mut cur: Value = serde_json::from_str(&self.now.to_string()).unwrap_or(v.clone());
                cur["status"] = v["status"].clone();
                cur["banner"] = v["banner"].clone();
                cur["down"] = json!(false);
                self.set_now(q(&cur.to_string()));
            }
            Msg::Albums { rows, note } => {
                let rows: Vec<Value> = rows
                    .iter()
                    .map(|a| {
                        json!({
                            "title": a.title,
                            "sub": join(a.artist.as_deref(), a.availability().as_deref()).unwrap_or_default(),
                            "uri": a.uri,
                            "onDisk": a.on_disk(),
                        })
                    })
                    .collect();
                self.set_albums(q(&json!({"rows": rows, "note": note.unwrap_or_default()}).to_string()));
            }
            Msg::Album { uri, title, rows } => {
                let rows: Vec<Value> = rows
                    .iter()
                    .map(|s| {
                        json!({
                            "title": s.title,
                            "sub": join(s.artist.as_deref(), s.availability).unwrap_or_default(),
                            "uri": s.uri,
                            "onDisk": s.on_disk(),
                        })
                    })
                    .collect();
                self.as_mut().set_album(q(&json!({"uri": uri, "title": title, "rows": rows}).to_string()));
                self.album_opened();
            }
            Msg::Search { query, result } => {
                if self.search_query != query {
                    return; // a late answer for a query since retyped
                }
                let v = match result {
                    Err(why) => json!({"query": query, "error": why, "hits": []}),
                    Ok(hits) => {
                        let hits: Vec<Value> = hits
                            .iter()
                            .map(|h| {
                                json!({
                                    "kind": match h.kind { HitKind::Artist => "artist", HitKind::Album => "album", HitKind::Song => "song" },
                                    "title": h.title,
                                    "sub": h.subtitle.clone().unwrap_or_default(),
                                    "uri": h.uri,
                                })
                            })
                            .collect();
                        json!({"query": query, "error": "", "hits": hits})
                    }
                };
                self.set_search(q(&v.to_string()));
            }
            Msg::Art { file, bytes } => {
                if self.art_file.as_deref() != Some(file.as_str()) {
                    return; // a late answer for a track that has since changed
                }
                let got = bytes.as_deref().and_then(cover_of);
                let url = got.as_ref().map(|(_, u)| u.clone()).unwrap_or_default();
                self.as_mut().rust_mut().cover_avg = got.map(|(a, _)| a);
                self.as_mut().set_cover(url);
                self.restyle();
            }
            Msg::Say(text) => self.say(&text),
            Msg::Confirm { token, steps, note } => {
                let mut body = steps.join("\n");
                if let Some(n) = note {
                    body.push_str("\n\n");
                    body.push_str(&n);
                }
                self.confirm_plan(q(&token), q(&body));
            }
            Msg::ConfirmCommand { command, question } => self.confirm_command(q(&command), q(&question)),
            Msg::ShowPlaying => self.show_playing(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypodj_client::model::now_playing;

    fn p(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Paused resumes with `pause 0` (from the playhead), never `play`.
    #[test]
    fn paused_resumes_from_the_playhead_and_up_next_starts_after_the_current_row() {
        let status = p(&[("state", "pause"), ("song", "0"), ("volume", "40")]);
        let cur = p(&[("file", "song/1"), ("Title", "One")]);
        let queue = hypodj_client::model::parse_queue(&p(&[
            ("file", "song/1"), ("Title", "One"), ("Pos", "0"),
            ("file", "song/2"), ("Title", "Two"), ("Pos", "1"),
        ]));
        let v = now_json(&now_playing(&status, &cur), &queue);
        assert_eq!(v["verb"], "pause 0");
        assert_eq!(v["volume"], 40);
        assert_eq!(v["upNext"][0]["title"], "Two");
        assert_eq!(v["upNext"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn nothing_playing_says_so_in_words() {
        let v = now_json(&now_playing(&[], &[]), &[]);
        assert_eq!(v["title"], "Nothing playing");
        assert_eq!(v["verb"], "play");
        assert_eq!(v["volume"], -1);
    }
}
