//! The reply catalog the client speaks, locked to the handler's own types.
//!
//! Every `PlanOutcome` render and every ACK body these commands emit has to
//! appear in `hypodj_client::spoken::reply_shapes`. A new outcome variant
//! fails the match below until a sample is added, and a drifted sentence
//! fails the string compare.

use hypodj_client::spoken::{reply_shapes, Act, State};
use hypodj_core::handler::{
    effect_set_volume, PlanOutcome, ACK_NL_INVALID, ACK_NL_NO_TOKEN, ACK_NL_STALE, ACK_NO_PLAN,
    ACK_TRANSLATOR_TASK, ACK_WAKE_DURATION, ACK_WAKE_TIME, EFFECT_FADING, EFFECT_NOTHING,
    EFFECT_PAUSED, EFFECT_STOPPED, EFFECT_WAKING, PAIR_JDID, PAIR_NL_ANSWER, PAIR_PLAN_ID,
    PAIR_RESULT, PAIR_SLEEP, PAIR_SLEEP_REMAINING, PAIR_WAKE, PAIR_WAKE_REMAINING,
};
use hypodj_core::nl::NlError;
use hypodj_core::plan::{PlanError, PlayMode};

fn outcome_lines() -> Vec<String> {
    let samples = [
        PlanOutcome::Added {
            n: 5,
            selector: "\"jazz\"".into(),
        },
        PlanOutcome::Added {
            n: 1,
            selector: "\"jazz\"".into(),
        },
        PlanOutcome::Added {
            n: 0,
            selector: "\"jazz\"".into(),
        },
        PlanOutcome::Played {
            n: 1,
            title: Some("\"Blue\"".into()),
            selector: "\"jazz\"".into(),
        },
        PlanOutcome::Played {
            n: 2,
            title: None,
            selector: "\"jazz\"".into(),
        },
        PlanOutcome::Played {
            n: 0,
            title: None,
            selector: "\"jazz\"".into(),
        },
        PlanOutcome::Removed(2),
        PlanOutcome::Curated {
            removed: 2,
            holding: "remixes".into(),
        },
        PlanOutcome::Moved(2),
        PlanOutcome::Cleared(3),
        PlanOutcome::Jumped(1),
        PlanOutcome::Jumped(0),
        PlanOutcome::Effect(EFFECT_FADING.into()),
        PlanOutcome::Effect(EFFECT_STOPPED.into()),
        PlanOutcome::Effect(EFFECT_PAUSED.into()),
        PlanOutcome::Effect(effect_set_volume(30)),
        PlanOutcome::Effect(EFFECT_WAKING.into()),
        PlanOutcome::Effect(EFFECT_NOTHING.into()),
        PlanOutcome::Failed("gone".into()),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Random.word(),
            true,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Random.word(),
            false,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Repeat.word(),
            true,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Repeat.word(),
            false,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Single.word(),
            true,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Single.word(),
            false,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Consume.word(),
            true,
        )),
        PlanOutcome::Effect(hypodj_core::handler::effect_mode(
            PlayMode::Consume.word(),
            false,
        )),
    ];
    for sample in &samples {
        match sample {
            PlanOutcome::Added { .. }
            | PlanOutcome::Played { .. }
            | PlanOutcome::Removed(_)
            | PlanOutcome::Curated { .. }
            | PlanOutcome::Moved(_)
            | PlanOutcome::Cleared(_)
            | PlanOutcome::Jumped(_)
            | PlanOutcome::Effect(_)
            | PlanOutcome::Failed(_) => {}
        }
    }
    samples.into_iter().map(|s| s.render()).collect()
}

#[test]
fn the_catalog_speaks_the_handlers_sentences() {
    let shapes = reply_shapes();
    let results: Vec<&str> = shapes
        .iter()
        .filter_map(|s| {
            s.pairs
                .iter()
                .find(|(k, _)| k == "result")
                .map(|(_, v)| v.as_str())
        })
        .collect();
    for sample in [
        PlanOutcome::Jumped(0),
        PlanOutcome::Failed("gone".into()),
        PlanOutcome::Added { n: 0, selector: "\"jazz\"".into() },
    ] {
        let line = sample.render();
        let spoken = hypodj_client::spoken::from_reply(
            &[("result".into(), line.clone())],
            None,
        );
        assert_eq!(spoken.state, State::Refused, "{line}");
        assert_eq!(spoken.text(), line);
    }
    for line in outcome_lines() {
        assert!(
            results.contains(&line.as_str()),
            "plan add result {line:?} is not in the catalog"
        );
    }

    let acks = [
        ACK_NL_NO_TOKEN.to_string(),
        format!("{ACK_NL_STALE}: {}", PlanError::PastDeadline),
        format!("{ACK_NL_INVALID}: {}", PlanError::Unresolvable),
        ACK_TRANSLATOR_TASK.to_string(),
        NlError::NotUnderstood.to_string(),
        ACK_WAKE_DURATION.to_string(),
        ACK_WAKE_TIME.to_string(),
        PlanError::PastDeadline.to_string(),
        ACK_NO_PLAN.to_string(),
    ];
    for ack in &acks {
        assert!(
            shapes
                .iter()
                .any(|s| s.ack.as_deref() == Some(ack.as_str())),
            "ack {ack:?} is not in the catalog"
        );
    }

    let keys = [
        PAIR_PLAN_ID,
        PAIR_RESULT,
        PAIR_SLEEP_REMAINING,
        PAIR_SLEEP,
        PAIR_WAKE_REMAINING,
        PAIR_WAKE,
        PAIR_NL_ANSWER,
        PAIR_JDID,
        hypodj_dsl::WINDDOWN_KEY,
        hypodj_dsl::WINDDOWN_WHEN_KEY,
        "X-hypodj-winddown-remaining",
    ];
    for shape in &shapes {
        for (k, _) in &shape.pairs {
            assert!(
                keys.contains(&k.as_str()),
                "{} uses a pair key the handler does not send: {k}",
                shape.name
            );
        }
    }
    assert!(shapes.iter().any(|s| {
        s.state == State::Armed
            && s.pairs == [(PAIR_PLAN_ID.into(), "4".into())]
            && s.lines == ["armed (plan 4)".to_string()]
    }));
    assert!(shapes
        .iter()
        .any(|s| { s.pairs.iter().filter(|(k, _)| k == PAIR_PLAN_ID).count() == 2 }));
    assert!(shapes.iter().any(|s| {
        s.pairs
            .iter()
            .any(|(k, v)| k == PAIR_RESULT && v == "added 5 tracks")
    }));
    assert!(shapes
        .iter()
        .any(|s| s.pairs.iter().any(|(k, _)| k == PAIR_SLEEP_REMAINING)));
    assert!(shapes
        .iter()
        .any(|s| s.pairs.iter().any(|(k, v)| k == PAIR_SLEEP && v == "none")));
    assert!(shapes
        .iter()
        .any(|s| s.pairs.iter().any(|(k, _)| k == PAIR_WAKE_REMAINING)));
    assert!(shapes
        .iter()
        .any(|s| s.pairs.iter().any(|(k, v)| k == PAIR_WAKE && v == "none")));
    assert!(shapes
        .iter()
        .any(|s| s.pairs.iter().any(|(k, _)| k == PAIR_NL_ANSWER)));
    assert!(shapes
        .iter()
        .any(|s| s.act == Some(Act::Clear) && s.state == State::Live));
    assert!(shapes.iter().any(|s| s.act == Some(Act::SleepCancel)));
    assert!(shapes.iter().any(|s| s.act == Some(Act::WinddownCancel)));
    assert!(shapes.iter().any(|s| s.act == Some(Act::WakeCancel)));
}
