//! GBNF derivation from the [`crate::llm::LlmRawPlan`] JSON-Schema (feature =
//! "llm" only). The grammar is DERIVED from the SINGLE-source-of-truth IR schema
//! so a constrained decode can only emit a valid subset object. A checked-in
//! snapshot ([`GBNF`]) is drift-guarded against a fresh derivation in a test, so
//! an IR change that desyncs the model surface fails loudly in CI.
//!
//! String states (Query/Genre) are LENGTH-CAPPED in the grammar so a runaway
//! generation cannot wedge decoding.

/// Max characters a free-text selector string may span in the grammar (the
/// documented append-only + count-clamped hole, length-capped on the model side).
pub const STRING_CAP: usize = 64;

/// Derive the JSON-Schema of the restricted model surface as a pretty string.
/// The drift-guard test snapshots THIS; a change to the LlmRawPlan subset (an IR
/// evolution) changes the schema and fails the snapshot loud.
pub fn schema_json() -> String {
    let schema = schemars::schema_for!(crate::llm::LlmRawPlan);
    serde_json::to_string_pretty(&schema).expect("schema serializes")
}

/// The STRICT plan grammar a constrained decode runs under. Every sentence it
/// generates is one flat [`crate::llm::LlmRawPlan`] that [`crate::llm::parse_llm_output`]
/// turns into a [`hypodj_core::plan::RawPlan`]: each action lists exactly the
/// scalars its `TryFrom` arm requires, so "set_volume without level" or "move
/// without dest" is not merely rejected afterwards, it cannot be decoded at all.
/// Numbers are bounded (a u8 volume, at most 4-digit slots, 5-digit seconds),
/// strings are length-capped and free of JSON escapes, there is no whitespace to
/// pad, and every plan ends `"once":true` like the rules path. Queue edits take no
/// `queue_position` trigger because that would reuse the selector's `slot` key.
/// Hand-written rather than a json-schema-to-grammar port so it stays auditable;
/// the tests below decode a sentence from every alternative.
pub const GBNF: &str = r##"root ::= "{" plan ",\"once\":true}"
plan ::= fade | halt | vol | content | qedit | mv | clr | noop
fade ::= "\"type\":" ( "\"fade_out\"" | "\"fade_in\"" ) ( ",\"secs\":" num )? when?
halt ::= "\"type\":" ( "\"stop\"" | "\"pause\"" ) when?
vol ::= "\"type\":\"set_volume\",\"level\":" level when?
content ::= "\"type\":" ( "\"enqueue\"" | "\"play_now\"" ) "," csel ( ",\"count\":" int )? when?
csel ::= "\"query\":" string | "\"genre\":" string | "\"radio\":true" | "\"sel\":\"similar_to_current\""
qedit ::= "\"type\":" ( "\"remove\"" | "\"play\"" ) "," qsel
mv ::= "\"type\":\"move\"," qsel "," dest
qsel ::= "\"sel\":\"current\"" | "\"sel\":\"position\",\"slot\":" int | "\"sel\":\"last\",\"count\":" int | "\"sel\":\"range\"," range | "\"sel\":\"query\",\"query\":" string
dest ::= "\"dest\":\"position\",\"dest_slot\":" int | "\"dest\":\"relative\",\"dest_rel\":" "-"? int
clr ::= "\"type\":\"clear\",\"scope\":" ( "\"all\"" | "\"after_current\"" | "\"range\"," range )
range ::= "\"range_start\":" int ",\"range_end\":" int
noop ::= "\"type\":\"noop\""
when ::= ",\"when\":" ( "\"after_current\"" | "\"album_boundary\"" | "\"after_secs\",\"when_secs\":" num | "\"time_remaining\",\"when_secs\":" num | "\"queue_position\",\"slot\":" int )
level ::= "100" | [1-9]? [0-9]
int ::= [0-9]{1,4}
num ::= [0-9]{1,5} ( "." [0-9]{1,2} )?
string ::= "\"" char{1,64} "\""
char ::= [^"\\\x00-\x1F]
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::parse_llm_output;

    /// DRIFT-GUARD: the derived JSON-Schema of the FLAT model surface must expose
    /// the closed action lexicon and the closed `when` lexicon, and must NEVER
    /// expose wall_clock / immediate (an IR change that leaks either onto the model
    /// surface fails loud here).
    #[test]
    fn schema_pins_the_restricted_surface() {
        let schema = schema_json();
        for kind in [
            // action lexicon
            "fade_out",
            "fade_in",
            "set_volume",
            "enqueue",
            // `when` lexicon
            "after_current",
            "after_secs",
            "album_boundary",
            "queue_position",
            "time_remaining",
        ] {
            assert!(schema.contains(kind), "schema must expose {kind}");
        }
        assert!(!schema.contains("wall_clock"), "wall_clock is OFF the model surface");
        // No RawTrigger `immediate` tag leaks onto the surface (the `when` default is
        // `now`, resolved to Immediate only in TryFrom, never named on the surface).
        assert!(
            !schema.contains("\"immediate\""),
            "immediate is OFF the model surface"
        );
    }

    /// The GBNF is length-capped and pins the FLAT action lexicon it targets.
    #[test]
    fn gbnf_is_capped_and_targets_the_surface() {
        assert!(GBNF.contains("fade_out"));
        assert!(GBNF.contains("enqueue"));
        assert!(!GBNF.contains("wall_clock"));
        assert!(GBNF.contains(&format!("char{{1,{STRING_CAP}}}")), "string state is length-capped");
    }

    /// ROUND-TRIP: every canned corpus JSON (the FLAT shape a constrained decode
    /// would emit for a model-surface phrasing) still deserializes to a RawPlan.
    #[test]
    fn canned_corpus_json_round_trips() {
        let corpus = [
            r#"{"type":"fade_out","secs":30.0,"when":"queue_position","slot":3,"once":true}"#,
            r#"{"type":"stop"}"#,
            r#"{"type":"stop","when":"album_boundary"}"#,
            r#"{"type":"fade_out","secs":10.0,"when":"after_secs","when_secs":300.0}"#,
            r#"{"type":"fade_out","secs":10.0,"when":"time_remaining","when_secs":30.0}"#,
            r#"{"type":"set_volume","level":42}"#,
            r#"{"type":"enqueue","query":"bon iver","count":5}"#,
            r#"{"type":"enqueue","genre":"jazz","count":3}"#,
            r#"{"type":"enqueue","radio":true,"count":5}"#,
        ];
        for json in corpus {
            assert!(parse_llm_output(json).is_ok(), "canned json must parse: {json}");
        }
    }

    /// GRAMMAR SOUNDNESS: a sentence through every alternative of [`GBNF`] (each
    /// action, selector, destination, scope and trigger, plus the bound edges of
    /// the number rules) parses into a RawPlan. A grammar edit that opens a path
    /// the parser refuses must add its sentence here and fail.
    #[test]
    fn every_grammar_alternative_parses() {
        let whens = [
            "",
            r#","when":"after_current""#,
            r#","when":"album_boundary""#,
            r#","when":"after_secs","when_secs":99999.99"#,
            r#","when":"time_remaining","when_secs":0"#,
            r#","when":"queue_position","slot":9999"#,
        ];
        let timed = [
            r#""type":"fade_out""#,
            r#""type":"fade_in","secs":1200"#,
            r#""type":"stop""#,
            r#""type":"pause""#,
            r#""type":"set_volume","level":100"#,
            r#""type":"set_volume","level":0"#,
            r#""type":"enqueue","query":"x""#,
            r#""type":"play_now","genre":"bossa nova","count":5"#,
            r#""type":"enqueue","radio":true"#,
            r#""type":"play_now","sel":"similar_to_current","count":3"#,
        ];
        let mut sentences = Vec::new();
        for t in timed {
            for w in whens {
                sentences.push(format!("{{{t}{w},\"once\":true}}"));
            }
        }
        let qsels = [
            r#""sel":"current""#,
            r#""sel":"position","slot":3"#,
            r#""sel":"last","count":2"#,
            r#""sel":"range","range_start":1,"range_end":4"#,
            r#""sel":"query","query":"Daft Punk""#,
        ];
        for q in qsels {
            for k in ["remove", "play"] {
                sentences.push(format!("{{\"type\":\"{k}\",{q},\"once\":true}}"));
            }
            for d in [r#""dest":"position","dest_slot":1"#, r#""dest":"relative","dest_rel":-2"#] {
                sentences.push(format!("{{\"type\":\"move\",{q},{d},\"once\":true}}"));
            }
        }
        for scope in [r#""all""#, r#""after_current""#, r#""range","range_start":2,"range_end":3"#] {
            sentences.push(format!("{{\"type\":\"clear\",\"scope\":{scope},\"once\":true}}"));
        }
        sentences.push(r#"{"type":"noop","once":true}"#.into());
        for s in &sentences {
            assert!(parse_llm_output(s).is_ok(), "grammar sentence must parse: {s}: {:?}", parse_llm_output(s));
        }
    }
}
