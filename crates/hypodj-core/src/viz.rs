//! The cosmetic HUD visualizer side-channel: a DEDICATED TCP socket at
//! `MPD_port + 1` that streams post-decode audio LEVELS to any subscriber.
//!
//! This mirrors the client's dedicated-socket precedent (the idle-push / albumart
//! sockets): the MPD command socket carries the owner-scoped `nl` handshake and a
//! one-shot `idle`, neither of which can host a ~20 fps level stream. Viz is out of
//! band, so `ADVERTISED_MPD_VERSION` is untouched.
//!
//! ## Hard bar: viz must NEVER disrupt audio
//!
//! The daemon-side level source is a NON-FATAL labelled `astats` af node (see
//! [`crate::player`]); a viz-socket error only ever closes that one connection. The
//! stream rides a DEDICATED `broadcast` channel (NOT the shared `DjEvent`
//! broadcast), so its ~20 fps churn cannot raise `Lagged` for other subscribers.
//!
//! ## Kept deliberately simple (per the design critique)
//!
//! ~220 B/s of levels needs no ceremony: NO capability-command negotiation, NO
//! proto-versioning, NO binary header. A one-line-per-frame text protocol is the P1
//! shape - debuggable (`nc host 6602` prints levels) and endian-free. Discovery is
//! derive-not-negotiate: the client connects to `MPD_port + 1` and treats
//! connection-refused as the clean "old daemon / no viz" degrade signal.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Semaphore};

/// Capacity of the dedicated viz broadcast. Small: viz is inherently latest-wins,
/// so a briefly-stalled writer just drops to the newest frame on `Lagged` rather
/// than resubscribing. A handful of frames of slack absorbs scheduling jitter.
pub const VIZ_BROADCAST_CAP: usize = 8;

/// The per-connection greeting. A client reads this first and, seeing it, knows it
/// reached a viz-capable daemon; anything else (or a refused connect) means fall
/// back to the decorative wave.
pub const VIZ_GREETING: &str = "OK HYPODJ-VIZ 1";

/// Idle keepalive period. A viz connection that has written nothing for this long
/// emits [`VIZ_PING`], so the wire is NEVER silent longer than this - well under
/// the client's 5s read timeout, and entirely independent of whether audio is
/// decoding. The heartbeat lives in the CONNECTION task, deliberately not in the
/// director/publisher: tying liveness to playback state is the very coupling that
/// let a paused daemon look dead (and pin every dead peer's fd).
pub const VIZ_HEARTBEAT: Duration = Duration::from_secs(2);

/// The idle keepalive line. Deliberately NOT a resting frame: a fake level line
/// would poison a client's render envelope, whereas an undecodable line provably
/// cannot - [`decode_frame`] returns `None` for it and every client already skips
/// a line that does not decode.
pub const VIZ_PING: &str = "PING";

/// Max concurrent viz subscribers. Viz legitimately has one or two local HUDs, so
/// a bound this loose never touches real use - but it means this cosmetic side
/// channel can never walk the whole process fd budget and take the MPD listener
/// (the actual product) down with it, honoring the "viz must NEVER disrupt audio"
/// bar on the resource axis and not just the error axis.
pub const VIZ_MAX_CONNS: usize = 32;

/// How long an accept loop sleeps after a failed `accept`. `EMFILE` is a
/// persistent saturation state, not a transient: with the listener still readable
/// a bare `continue` spins at full speed (an observed 5h55m of CPU and a log
/// flood). 250ms caps the flood at ~4 lines/s and costs at most 250ms of accept
/// latency after a spurious error.
pub const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(250);

/// One post-decode level frame published on the viz broadcast. `rms_db`/`peak_db`
/// are RAW (pre-softvol) dBFS; `gain_db` is the daemon's current softvol gain, so a
/// client recovers the audible post-gain level as `rms_db + gain_db`. `playing`
/// gates the client between the live field and the resting hairline.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VizFrame {
    pub rms_db: f32,
    pub peak_db: f32,
    pub gain_db: f32,
    pub playing: bool,
}

/// Serialize one frame to a single newline-terminated wire line:
/// `<rms> <peak> <gain> <playing>\n` (dBFS at 2 decimals, playing as 0/1). Pure and
/// unit-tested; the exact inverse of [`decode_frame`].
pub fn encode_frame(f: &VizFrame) -> String {
    format!(
        "{:.2} {:.2} {:.2} {}\n",
        f.rms_db,
        f.peak_db,
        f.gain_db,
        if f.playing { 1 } else { 0 }
    )
}

/// Parse one wire line (without or with a trailing newline) back into a
/// [`VizFrame`]. Returns `None` on any malformed line so a partial/garbled frame is
/// simply skipped, never a panic. Pure and unit-tested.
pub fn decode_frame(line: &str) -> Option<VizFrame> {
    let mut it = line.split_whitespace();
    let rms_db = it.next()?.parse::<f32>().ok()?;
    let peak_db = it.next()?.parse::<f32>().ok()?;
    let gain_db = it.next()?.parse::<f32>().ok()?;
    let playing = match it.next()? {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    if it.next().is_some() {
        return None; // trailing garbage: reject rather than half-trust it.
    }
    Some(VizFrame { rms_db, peak_db, gain_db, playing })
}

/// Serve the viz side-channel: accept connections on `bind` and stream every
/// broadcast frame to each. This is spawned best-effort by the daemon; a bind error
/// is returned (logged by the caller) and simply means no viz socket - playback and
/// the MPD server are entirely unaffected.
pub async fn serve_viz(bind: SocketAddr, frames: broadcast::Sender<VizFrame>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "viz socket listening");
    serve_viz_on(listener, frames).await
}

/// The accept loop proper, over an already-bound listener (so a test can bind port
/// 0 and learn the ephemeral port). Every accepted connection holds one of
/// [`VIZ_MAX_CONNS`] permits for its lifetime; over the cap the socket is dropped
/// immediately so the backlog cannot fill.
async fn serve_viz_on(
    listener: TcpListener,
    frames: broadcast::Sender<VizFrame>,
) -> anyhow::Result<()> {
    let permits = Arc::new(Semaphore::new(VIZ_MAX_CONNS));
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "viz accept failed");
                // Saturation, not a transient: sleep so a persistent error (EMFILE
                // against a still-readable listener) cannot spin this loop.
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        // Over the cap: close this one now rather than let a cosmetic channel
        // consume fds the MPD listener needs.
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::debug!(%peer, cap = VIZ_MAX_CONNS, "viz connection cap reached, dropping");
            drop(sock);
            continue;
        };
        // A fresh receiver per connection: an unsubscribed/old client never existed
        // here, and a wedged client closes only its own conn.
        let rx = frames.subscribe();
        tokio::spawn(async move {
            // Released when the connection task ends, whatever ends it.
            let _permit = permit;
            if let Err(e) = serve_viz_conn(sock, rx).await {
                tracing::debug!(%peer, error = %e, "viz connection closed");
            }
        });
    }
}

/// Drive one viz connection: write the greeting, then stream frames until the
/// socket closes. On `Lagged` we do NOT resubscribe (viz is latest-wins - continue
/// from the newest frame); on `Closed` the daemon is winding down, so we stop.
///
/// The connection owns its own liveness: the read half stays in the `select!` set
/// so a peer FIN ends the task the instant the kernel delivers it. It must never be
/// inferred from a future frame write - frames only flow while audio decodes, so a
/// write-only task pins the fd of every dead peer for as long as the deck is
/// paused. The heartbeat is the mirror of that: bounded-time idle traffic so the
/// client, likewise, never reads silence as death.
///
/// Generic over the stream so the heartbeat can be fake-clocked against an
/// in-memory duplex (a paused clock plus a real socket interleaves with the IO
/// driver and cannot pin down a tick); the daemon always passes a `TcpStream`.
async fn serve_viz_conn<S>(mut sock: S, mut rx: broadcast::Receiver<VizFrame>) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    sock.write_all(format!("{VIZ_GREETING}\n").as_bytes()).await?;
    sock.flush().await?;
    let (mut rd, mut wr) = tokio::io::split(sock);
    let mut scratch = [0u8; 64];
    let mut beat = tokio::time::interval(VIZ_HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    beat.tick().await; // the first tick is immediate; the wire just got the greeting.
    loop {
        tokio::select! {
            r = rd.read(&mut scratch) => match r {
                // FIN (or a read error): the peer is gone, so this fd goes back now
                // even if no frame ever flows again.
                Ok(0) | Err(_) => break,
                // Viz clients send nothing; tolerate an `nc` user typing by
                // DISCARDING the bytes, never interpreting them.
                Ok(_) => continue,
            },
            _ = beat.tick() => {
                // Nothing flowed for a full period: prove the daemon is alive.
                wr.write_all(format!("{VIZ_PING}\n").as_bytes()).await?;
                wr.flush().await?;
            }
            f = rx.recv() => match f {
                Ok(frame) => {
                    // Best-effort write; a broken pipe just ends this connection.
                    wr.write_all(encode_frame(&frame).as_bytes()).await?;
                    wr.flush().await?;
                    // A real frame IS idle traffic: push the next PING a full
                    // period out rather than interleaving one right behind it.
                    beat.reset();
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_through_the_wire() {
        let f = VizFrame { rms_db: -20.51, peak_db: -12.30, gain_db: -6.00, playing: true };
        let line = encode_frame(&f);
        assert!(line.ends_with('\n'), "frame is newline terminated");
        let back = decode_frame(line.trim_end()).expect("decodes");
        // 2-decimal wire precision: compare at that resolution.
        assert!((back.rms_db - f.rms_db).abs() < 0.005);
        assert!((back.peak_db - f.peak_db).abs() < 0.005);
        assert!((back.gain_db - f.gain_db).abs() < 0.005);
        assert!(back.playing);
    }

    #[test]
    fn decode_tolerates_and_rejects() {
        // A paused frame decodes with playing=false.
        let f = decode_frame("-54.00 -54.00 0.00 0").unwrap();
        assert!(!f.playing);
        assert_eq!(f.rms_db, -54.00);
        // Malformed lines are rejected (skipped), never a panic.
        assert!(decode_frame("").is_none());
        assert!(decode_frame("garbage").is_none());
        assert!(decode_frame("-1 -2 -3").is_none()); // too few fields
        assert!(decode_frame("-1 -2 -3 2").is_none()); // bad playing flag
        assert!(decode_frame("-1 -2 -3 1 extra").is_none()); // trailing garbage
        assert!(decode_frame("x -2 -3 1").is_none()); // non-numeric
    }

    /// Spawn `serve_viz_on` over an ephemeral loopback port and return the port
    /// plus the frame sender. Loopback only, so it runs in the certless sandbox.
    async fn spawn_viz() -> (u16, broadcast::Sender<VizFrame>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, _) = broadcast::channel(VIZ_BROADCAST_CAP);
        let frames = tx.clone();
        tokio::spawn(async move {
            let _ = serve_viz_on(listener, frames).await;
        });
        (port, tx)
    }

    /// Read one newline-terminated line from a client socket.
    async fn read_line(rd: &mut tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>) -> String {
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(rd, &mut line).await.expect("read line");
        line
    }

    /// Yield until `cond` holds, bounded by a task budget (never a wall clock).
    async fn until<F: FnMut() -> bool>(mut cond: F, what: &str) {
        for _ in 0..10_000 {
            if cond() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn conn_task_exits_on_peer_fin_with_zero_frames_flowing() {
        // THE leak case: the daemon is paused, so no frame is ever published. A
        // write-only task would only discover the FIN on a later write that never
        // comes, pinning the fd in CLOSE-WAIT forever.
        let (port, tx) = spawn_viz().await;
        let sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let (rd, _wr) = sock.into_split();
        let mut rd = tokio::io::BufReader::new(rd);
        let greeting = read_line(&mut rd).await;
        assert_eq!(greeting.trim_end(), VIZ_GREETING);
        until(|| tx.receiver_count() == 1, "the conn task to subscribe").await;

        // Drop the client WITHOUT publishing a single frame.
        drop(rd);
        drop(_wr);
        until(|| tx.receiver_count() == 0, "the conn task to exit on FIN").await;
    }

    #[tokio::test]
    async fn garbage_from_the_peer_is_discarded_and_frames_keep_flowing() {
        // An `nc` user typing must not be interpreted, and must not kill the conn.
        let (port, tx) = spawn_viz().await;
        let sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let (rd, mut wr) = sock.into_split();
        let mut rd = tokio::io::BufReader::new(rd);
        assert_eq!(read_line(&mut rd).await.trim_end(), VIZ_GREETING);
        until(|| tx.receiver_count() == 1, "the conn task to subscribe").await;

        wr.write_all(b"hello daemon\n").await.expect("write garbage");
        wr.flush().await.expect("flush");

        // The connection survives and still delivers the next frame (the broadcast
        // buffers it, so this does not depend on when the garbage was drained).
        let f = VizFrame { rms_db: -18.25, peak_db: -9.5, gain_db: -3.0, playing: true };
        tx.send(f).expect("publish");
        loop {
            let line = read_line(&mut rd).await;
            if line.trim_end() == VIZ_PING {
                continue; // a heartbeat may interleave; keep reading.
            }
            assert_eq!(decode_frame(line.trim_end()), Some(f));
            break;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn idle_conn_pings_within_the_heartbeat_and_a_frame_resets_it() {
        // Bounded-time idle traffic: a paused daemon (zero frames) must still put
        // a byte on the wire well inside the client's 5s read timeout. In-memory
        // duplex, so the paused clock is exact (a real socket interleaves the IO
        // driver with auto-advance and blurs the tick).
        let (client, server) = tokio::io::duplex(4096);
        let (tx, rx) = broadcast::channel(VIZ_BROADCAST_CAP);
        tokio::spawn(async move {
            let _ = serve_viz_conn(server, rx).await;
        });
        let (crd, _cwr) = tokio::io::split(client);
        let mut rd = tokio::io::BufReader::new(crd);

        let start = tokio::time::Instant::now();
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut rd, &mut line).await.expect("greeting");
        assert_eq!(line.trim_end(), VIZ_GREETING);
        assert_eq!(start.elapsed(), Duration::ZERO, "the greeting is immediate");

        // Publish nothing at all; the wire must still speak within one period.
        line.clear();
        tokio::io::AsyncBufReadExt::read_line(&mut rd, &mut line).await.expect("ping");
        assert_eq!(line.trim_end(), VIZ_PING, "an idle conn emits PING");
        assert!(decode_frame(line.trim_end()).is_none(), "PING is not a frame");
        assert_eq!(start.elapsed(), VIZ_HEARTBEAT, "one heartbeat period, exactly");

        // Mid-period, publish a real frame. It is the next line, and it RESETS the
        // heartbeat: the PING after it lands a full period after the frame, not at
        // the tick the frame interrupted (only half a period out).
        tokio::time::sleep(VIZ_HEARTBEAT / 2).await;
        let sent = tokio::time::Instant::now();
        let f = VizFrame { rms_db: -22.0, peak_db: -11.0, gain_db: -6.0, playing: true };
        tx.send(f).expect("publish");
        line.clear();
        tokio::io::AsyncBufReadExt::read_line(&mut rd, &mut line).await.expect("frame");
        assert_eq!(decode_frame(line.trim_end()), Some(f), "the frame, not a PING");
        assert_eq!(sent.elapsed(), Duration::ZERO, "the frame goes out at once");
        line.clear();
        tokio::io::AsyncBufReadExt::read_line(&mut rd, &mut line).await.expect("ping");
        assert_eq!(line.trim_end(), VIZ_PING);
        assert_eq!(
            sent.elapsed(),
            VIZ_HEARTBEAT,
            "the frame reset the heartbeat, so the next PING is a full period later"
        );
    }

    #[tokio::test]
    async fn viz_conns_are_capped_and_the_permit_returns_on_close() {
        let (port, tx) = spawn_viz().await;
        let mut held = Vec::new();
        for _ in 0..VIZ_MAX_CONNS {
            let sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            let (rd, wr) = sock.into_split();
            let mut rd = tokio::io::BufReader::new(rd);
            assert_eq!(read_line(&mut rd).await.trim_end(), VIZ_GREETING);
            held.push((rd, wr));
        }
        until(|| tx.receiver_count() == VIZ_MAX_CONNS, "all conns to subscribe").await;

        // One over the cap: accepted by the kernel, then dropped at once, so the
        // client reads EOF instead of a greeting.
        let extra = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let (extra_rd, extra_wr) = extra.into_split();
        let mut extra_rd = tokio::io::BufReader::new(extra_rd);
        let mut buf = String::new();
        let n = tokio::io::AsyncBufReadExt::read_line(&mut extra_rd, &mut buf).await.expect("read");
        assert_eq!(n, 0, "over the cap the socket is closed, not served");
        assert_eq!(tx.receiver_count(), VIZ_MAX_CONNS, "no task was spawned for it");
        drop(extra_rd);
        drop(extra_wr);

        // Free one: the permit is released when that task exits, so a fresh
        // connect is served again.
        held.pop();
        until(|| tx.receiver_count() == VIZ_MAX_CONNS - 1, "the closed conn to release").await;
        let again = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let (again_rd, _again_wr) = again.into_split();
        let mut again_rd = tokio::io::BufReader::new(again_rd);
        assert_eq!(read_line(&mut again_rd).await.trim_end(), VIZ_GREETING);
    }

    #[tokio::test]
    async fn negative_infinity_gain_encodes_finite_shape() {
        // A silence frame with a very low gain still round-trips as a finite,
        // parseable line (the wire never carries a NaN token).
        let f = VizFrame { rms_db: -120.0, peak_db: -120.0, gain_db: -60.0, playing: false };
        let back = decode_frame(encode_frame(&f).trim_end()).unwrap();
        assert_eq!(back.playing, false);
        assert!(back.rms_db <= -119.0);
    }
}
