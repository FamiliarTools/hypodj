//! SERVER REACHABILITY: whether the music server answers right now, as the daemon
//! last OBSERVED it.
//!
//! The daemon always knew (the startup greet logs `server_online=false`, every
//! failed browse ACKs) but said so nowhere a listener looks, so a dj-gui rendered the
//! identical first frame whether the server was up, blackholed or unroutable. This
//! is the one place that verdict lives: a tiny state slot fed by a bounded
//! heartbeat ping, read by `status` without ever touching the network.
//!
//! Two properties keep it honest:
//!
//! - BOUNDED TIME. Each probe is capped at [`PROBE_TIMEOUT`] on the injected clock,
//!   so a blackholed address (no SYN-ACK, no RST) reads `unreachable` within one
//!   interval plus that cap instead of after the kernel's SYN ladder.
//! - NEVER BLOCKS STATUS. The slot is a plain `std` mutex held for a copy; `status`
//!   reads the last verdict, it never waits for one.
//!
//! Generic over [`Clock`] and the probe, so the loop runs under
//! `#[tokio::test(start_paused)]` with no network and no wall clock.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::clock::Clock;

/// How often the heartbeat asks. Small enough that a server dropping mid-song is
/// named within ~10s (interval + timeout), cheap enough to run forever: one tiny
/// `ping.view` round trip.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// The cap on one probe. Under the metadata client's own 5s connect timeout, so the
/// heartbeat's verdict never waits on the kernel.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// What the last probe saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// No probe has finished yet.
    Unknown,
    /// The server answered a ping.
    Online,
    /// The server did not answer: a timeout, a refused or reset connection.
    Unreachable,
    /// This machine has no route to the server at all (ENETUNREACH/EHOSTUNREACH).
    NoNetwork,
    /// The server answered, but with an error (bad credentials, a broken reply).
    Error,
}

impl Reach {
    /// The wire word on `status` (`X-Server`).
    pub fn label(self) -> &'static str {
        match self {
            Reach::Unknown => "unknown",
            Reach::Online => "online",
            Reach::Unreachable => "unreachable",
            Reach::NoNetwork => "no-network",
            Reach::Error => "error",
        }
    }
}

/// The shared slot: the last verdict and when it last CHANGED.
pub struct ServerReach {
    slot: Mutex<(Reach, Instant)>,
}

impl ServerReach {
    pub fn new(now: Instant) -> Self {
        Self { slot: Mutex::new((Reach::Unknown, now)) }
    }

    /// Record a verdict. The "since" instant moves only on a change, so it reads as
    /// "how long has it been like this". Returns whether it changed.
    pub fn record(&self, r: Reach, now: Instant) -> bool {
        let mut s = self.slot.lock().expect("reach lock");
        if s.0 == r {
            return false;
        }
        *s = (r, now);
        true
    }

    /// The last verdict and when it began.
    pub fn snapshot(&self) -> (Reach, Instant) {
        *self.slot.lock().expect("reach lock")
    }
}

/// The heartbeat: probe, record, sleep to the next ABSOLUTE deadline (a slow probe
/// shortens the next sleep instead of drifting the cadence). Never returns.
pub async fn run<C, F, Fut>(reach: Arc<ServerReach>, clock: C, probe: F)
where
    C: Clock,
    F: Fn() -> Fut + Send,
    Fut: Future<Output = Reach> + Send,
{
    let mut next = clock.now();
    loop {
        let started = clock.now();
        let r = tokio::select! {
            r = probe() => r,
            _ = clock.sleep_until(started + PROBE_TIMEOUT) => Reach::Unreachable,
        };
        if reach.record(r, clock.now()) {
            tracing::info!(server = r.label(), "server reachability changed");
        }
        next += PROBE_INTERVAL;
        if next < clock.now() {
            next = clock.now();
        }
        clock.sleep_until(next).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TokioClock;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[tokio::test(start_paused = true)]
    async fn a_hung_probe_reads_unreachable_within_the_cap_and_recovers() {
        let reach = Arc::new(ServerReach::new(Instant::now()));
        // 0 = answers, 1 = hangs forever, 2 = no route.
        let mode = Arc::new(AtomicU8::new(0));
        let m = mode.clone();
        let task = tokio::spawn(run(reach.clone(), TokioClock, move || {
            let m = m.clone();
            async move {
                match m.load(Ordering::SeqCst) {
                    0 => Reach::Online,
                    1 => std::future::pending().await,
                    _ => Reach::NoNetwork,
                }
            }
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(reach.snapshot().0, Reach::Online);

        // Blackholed: the next probe hangs; the cap ends it before interval + timeout.
        mode.store(1, Ordering::SeqCst);
        tokio::time::sleep(PROBE_INTERVAL + PROBE_TIMEOUT + Duration::from_millis(10)).await;
        assert_eq!(reach.snapshot().0, Reach::Unreachable);

        mode.store(2, Ordering::SeqCst);
        tokio::time::sleep(PROBE_INTERVAL + Duration::from_millis(10)).await;
        assert_eq!(reach.snapshot().0, Reach::NoNetwork);

        mode.store(0, Ordering::SeqCst);
        tokio::time::sleep(PROBE_INTERVAL + Duration::from_millis(10)).await;
        assert_eq!(reach.snapshot().0, Reach::Online);
        task.abort();
    }

    #[test]
    fn since_moves_only_on_a_change() {
        let t0 = Instant::now();
        let r = ServerReach::new(t0);
        assert!(r.record(Reach::Online, t0 + Duration::from_secs(1)));
        assert!(!r.record(Reach::Online, t0 + Duration::from_secs(9)));
        assert_eq!(r.snapshot(), (Reach::Online, t0 + Duration::from_secs(1)));
    }
}
