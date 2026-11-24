//! Pending-plan PERSISTENCE across a daemon restart: the pure, file-format half.
//!
//! The decision (task 9omxa9z): a plan whose meaning is a CIVIL promise survives
//! a restart; a plan whose meaning is relative to this process's queue or timeline
//! does not.
//!   - [`RawTrigger::Daily`] (recurring) and [`RawTrigger::WallClock`] (an absolute
//!     civil instant, which is also what `sleep 1h` / `winddown 20m` / `wake at 7`
//!     arm) are persisted and re-armed on startup.
//!   - Everything else is dropped with the process: a queue-anchored trigger
//!     (track N, after this track, album boundary, time remaining) names queue ids
//!     that are re-minted on restore, and a short relative `in 5m` span is a
//!     "from now" that a restart would silently stretch.
//!
//! The fire-late policy for a one-shot wall-clock plan missed while the daemon
//! was down: within [`MISSED_GRACE`] it fires right after startup (a switch that
//! restarted hypodj across 07:00 must not swallow the alarm); beyond it the plan
//! is dropped with a warn (a wake firing at 11:00 because the daemon came back
//! late would be a startle, not a service). A recurring plan never "misses": it
//! simply re-arms to its next occurrence.
//!
//! Corruption safety mirrors [`crate::resume`]: [`from_json`] / [`load`] return
//! `None` on ANY error or schema mismatch and never panic, and the write goes
//! through the shared [`crate::resume::atomic_write_bytes`].
//!
//! Plan ids are NOT persisted: a restored plan gets a fresh id from this
//! process's monotonic counter (an id is a handle for a live registry, never a
//! durable name). The `reserved` flag IS persisted, so a restored `sleep` /
//! `winddown` / `wake` is still the daemon's single feature instance.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::plan::{RawPlan, RawTrigger};

/// The on-disk schema version; a mismatch on load is a clean empty registry.
pub const PLANS_SCHEMA_VERSION: u32 = 1;

/// How late a missed one-shot wall-clock plan may still fire on startup.
pub const MISSED_GRACE: chrono::Duration = chrono::Duration::minutes(5);

/// The persisted registry.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlansFile {
    pub schema_version: u32,
    pub plans: Vec<PersistedPlan>,
}

/// One persisted plan: the clamped raw plan plus whether the DAEMON armed it as
/// one of its reserved feature singletons (registry-side, never producer-set).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistedPlan {
    pub reserved: bool,
    pub raw: RawPlan,
}

/// Whether a plan with this trigger survives a restart (see the module doc).
pub fn is_persistable(t: &RawTrigger) -> bool {
    matches!(t, RawTrigger::WallClock { .. } | RawTrigger::Daily { .. })
}

/// The trigger a persisted plan re-arms with at `now_civil`, or `None` to drop it
/// (a one-shot missed by more than [`MISSED_GRACE`], or a non-persistable shape
/// that should never have been written).
pub fn restore_trigger(t: &RawTrigger, now_civil: chrono::DateTime<chrono::Utc>) -> Option<RawTrigger> {
    match t {
        RawTrigger::Daily { .. } => Some(t.clone()),
        RawTrigger::WallClock { at } if *at > now_civil => Some(t.clone()),
        RawTrigger::WallClock { at } if now_civil - *at <= MISSED_GRACE => Some(RawTrigger::WallClock {
            at: now_civil + chrono::Duration::seconds(1),
        }),
        _ => None,
    }
}

pub fn to_json(f: &PlansFile) -> serde_json::Result<String> {
    serde_json::to_string_pretty(f)
}

/// Parse a plans file; ANY error or a schema mismatch is `None`. Never panics.
pub fn from_json(raw: &str) -> Option<PlansFile> {
    match serde_json::from_str::<PlansFile>(raw) {
        Ok(f) if f.schema_version == PLANS_SCHEMA_VERSION => Some(f),
        _ => None,
    }
}

/// Load the persisted plans; missing / unreadable / corrupt / old is `None`.
pub fn load(path: &Path) -> Option<PlansFile> {
    let raw = std::fs::read_to_string(path).ok()?;
    let f = from_json(&raw);
    if f.is_none() {
        tracing::warn!(path = %path.display(), "invalid/old plans file; starting with no persisted plans");
    }
    f
}

/// Atomically write the registry (sibling temp, fsync, rename).
pub fn store_atomic(path: &Path, f: &PlansFile) -> std::io::Result<()> {
    let body = to_json(f).map_err(std::io::Error::other)?;
    crate::resume::atomic_write_bytes(path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, DaySet, FadeIntentIr, Selector};

    fn plan(trigger: RawTrigger, action: Action) -> RawPlan {
        RawPlan { version: 1, trigger, action, once: true, origin: "wake".into() }
    }

    // Round trip through the real file path: every persisted shape, including a
    // Wake carrying a free-text Query selector and a ToFloor fade, comes back
    // field-for-field (compared through serde, since RawPlan has no PartialEq).
    #[test]
    fn plans_file_round_trips_on_disk() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-24T07:00:00Z").unwrap().to_utc();
        let f = PlansFile {
            schema_version: PLANS_SCHEMA_VERSION,
            plans: vec![
                PersistedPlan {
                    reserved: true,
                    raw: plan(
                        RawTrigger::Daily { hour: 7, minute: 15, days: DaySet::WEEKDAYS },
                        Action::Wake { selector: Some(Selector::Query("bon iver".into())), count: 20 },
                    ),
                },
                PersistedPlan {
                    reserved: false,
                    raw: plan(
                        RawTrigger::WallClock { at },
                        Action::Fade(FadeIntentIr::ToFloor { secs: 600.0 }),
                    ),
                },
                PersistedPlan {
                    reserved: false,
                    raw: plan(RawTrigger::WallClock { at }, Action::SetVolume(30)),
                },
                PersistedPlan {
                    reserved: false,
                    raw: plan(
                        RawTrigger::WallClock { at },
                        Action::Move {
                            sel: crate::plan::QueueSelector::QueryMatch("miles".into()),
                            dest: crate::plan::MoveDest::Relative(-1),
                        },
                    ),
                },
                PersistedPlan {
                    reserved: false,
                    raw: plan(
                        RawTrigger::Daily { hour: 23, minute: 0, days: DaySet::EVERY },
                        Action::Enqueue { selector: Selector::Genre("jazz".into()), count: 5 },
                    ),
                },
            ],
        };
        let dir = std::env::temp_dir().join(format!("hypodj-plans-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plans.json");
        store_atomic(&path, &f).unwrap();
        let back = load(&path).expect("loads");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&f).unwrap()
        );
        assert!(back.plans[0].reserved && !back.plans[1].reserved);
    }

    // Corruption and version mismatch are a clean None, never a panic.
    #[test]
    fn corrupt_or_old_file_is_none() {
        assert!(from_json("").is_none());
        assert!(from_json("{not json").is_none());
        assert!(from_json(r#"{"schema_version":99,"plans":[]}"#).is_none());
        assert!(from_json(r#"{"schema_version":1,"plans":[{"reserved":false}]}"#).is_none());
        assert!(load(Path::new("/nonexistent/hypodj/plans.json")).is_none());
    }

    // The persistence boundary and the fire-late policy.
    #[test]
    fn persistable_set_and_restore_policy() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-24T07:02:00Z").unwrap().to_utc();
        let daily = RawTrigger::Daily { hour: 7, minute: 0, days: DaySet::EVERY };
        assert!(is_persistable(&daily));
        assert!(is_persistable(&RawTrigger::WallClock { at: now }));
        assert!(!is_persistable(&RawTrigger::SpanElapsed { secs: 60.0 }));
        assert!(!is_persistable(&RawTrigger::TrackAfterCurrent));
        assert!(!is_persistable(&RawTrigger::Immediate));

        // Recurring: always re-arms.
        assert!(matches!(restore_trigger(&daily, now), Some(RawTrigger::Daily { .. })));
        // Future one-shot: unchanged.
        let later = now + chrono::Duration::hours(1);
        match restore_trigger(&RawTrigger::WallClock { at: later }, now) {
            Some(RawTrigger::WallClock { at }) => assert_eq!(at, later),
            other => panic!("{other:?}"),
        }
        // Missed by 2 minutes (inside the grace): fires right after startup.
        let missed = now - chrono::Duration::minutes(2);
        match restore_trigger(&RawTrigger::WallClock { at: missed }, now) {
            Some(RawTrigger::WallClock { at }) => assert!(at > now && at - now <= chrono::Duration::seconds(1)),
            other => panic!("{other:?}"),
        }
        // Missed by an hour: dropped.
        let stale = now - chrono::Duration::hours(1);
        assert!(restore_trigger(&RawTrigger::WallClock { at: stale }, now).is_none());
        assert!(restore_trigger(&RawTrigger::SpanElapsed { secs: 60.0 }, now).is_none());
    }
}

#[cfg(test)]
mod adversary_repro {
    use super::*;
    use crate::plan::{next_daily_fire, DaySet};
    use chrono::TimeZone;

    #[test]
    fn restart_across_seven_swallows_daily_but_not_oneshot() {
        let seven = chrono::Utc.with_ymd_and_hms(2026, 11, 24, 7, 0, 0).unwrap();
        let back_up = seven + chrono::Duration::minutes(1);
        let one = restore_trigger(&RawTrigger::WallClock { at: seven }, back_up);
        println!("one-shot restored as: {one:?}");
        let daily = RawTrigger::Daily { hour: 7, minute: 0, days: DaySet::EVERY };
        let restored = restore_trigger(&daily, back_up).unwrap();
        let RawTrigger::Daily { hour, minute, days } = restored else { panic!() };
        let next = next_daily_fire(&chrono::Utc, hour, minute, days, back_up).unwrap();
        println!("daily restored fires at: {next}");
        assert!(next - back_up <= MISSED_GRACE, "today's 07:00 daily was swallowed; next fire {next}");
    }
}
