//! The `plan add` DSL value grammar - quoting and its inverse, and nothing else.
//!
//! This crate exists so there is EXACTLY ONE implementation of the quoting rule.
//! The daemon's echo ([`hypodj_core::echo`]) renders a plan back to the DSL; an
//! out-of-process producer (a stdio MCP server, a script) builds the same line
//! from structured fields. Both must agree with the daemon's tokenizer character
//! for character, and the producer cannot depend on `hypodj-core` because that
//! crate hard-links `libmpv2`. So the rule lives here, with zero dependencies,
//! and both sides import it.
//!
//! [`tokenize`] is a copy of the daemon's `plan add` line tokenizer
//! (`hypodj-core/src/mpd.rs`), carried here ONLY so the round-trip
//! `tokenize(dsl_value(x)) == [x]` is testable in the crate that owns the rule.

/// Quote + escape a selector value so a multi-word value ("good vibes", "drum
/// and bass") survives the `plan add` tokenizer as ONE token (it splits on
/// unquoted whitespace and unescapes `\"`/`\\` inside quotes). A single bare word
/// is emitted verbatim; anything with whitespace/quote/backslash is quoted.
///
/// Returns `None` when the value contains a control character (newline, CR, tab,
/// etc.). The `plan add <dsl>` line is framed on the wire by a literal `\n`, and
/// the tokenizer's `\`-escape only unescapes the NEXT char (never re-encodes a
/// literal newline), so a value carrying a newline would smuggle EXTRA command
/// lines onto the wire and desynchronise the frame. Refusing to render it makes
/// the caller fall back to direct arming (or a loud "cannot express" miss) - a
/// control char is nonsense in a music selector anyway.
pub fn dsl_value(s: &str) -> Option<String> {
    if s.chars().any(|c| c.is_control()) {
        return None;
    }
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\') {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        Some(out)
    } else {
        Some(s.to_string())
    }
}

/// Split a DSL line into tokens exactly the way the daemon does: whitespace
/// separates, a `"` opens a quoted run in which `\` escapes the next character
/// literally. Unquoted runs take `\` literally (no escape), which is why
/// [`dsl_value`] quotes anything carrying a backslash.
///
/// This is the inverse [`dsl_value`] is checked against; the daemon keeps its own
/// copy (it also splits the command name off the front), and the round-trip test
/// below is what keeps the two honest.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut toks: Vec<String> = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        // skip whitespace
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            None => break,
            Some('"') => {
                chars.next();
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                s.push(n);
                            }
                        }
                        _ => s.push(c),
                    }
                }
                toks.push(s);
            }
            Some(_) => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                toks.push(s);
            }
        }
    }
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture set the round trip is asserted over: bare words, spaces of
    /// every kind, embedded quotes, backslashes, and the empty string.
    const ROUND_TRIP: &[&str] = &[
        "blue",
        "good vibes",
        "drum and bass",
        "  leading and trailing  ",
        "so what",
        r#"say "hello""#,
        r#""quoted whole""#,
        r"back\slash",
        r"trailing\\",
        r#"both \ and " together"#,
        "",
        "\"",
        "\\",
        "unicode caf\u{e9} \u{2013} ok",
        // The words the daemon's own post-tokenize modifier strippers key on:
        // quoting must at least keep them ONE token here (the daemon's stripper
        // is a separate, known limitation).
        "origin",
        "once",
    ];

    // dsl_value is the inverse of tokenize over every fixture: exactly one token
    // comes back, byte-identical to the input.
    #[test]
    fn dsl_value_round_trips_through_tokenize() {
        for v in ROUND_TRIP {
            let rendered = dsl_value(v).unwrap_or_else(|| panic!("refused a printable value: {v:?}"));
            let toks = tokenize(&rendered);
            assert_eq!(toks, vec![v.to_string()], "round trip failed for {v:?} (rendered {rendered:?})");
        }
    }

    // A value carrying a control char is REFUSED, never rendered - that is what
    // keeps a newline from framing a second command line onto the wire.
    #[test]
    fn control_chars_are_refused() {
        for v in ["a\nsetvol 100", "a\rb", "tab\there", "nul\0", "bell\u{7}", "\u{85}next-line"] {
            assert!(dsl_value(v).is_none(), "should have refused {v:?}");
        }
    }

    // A bare single word is emitted verbatim (no gratuitous quotes), which is what
    // keeps the rendered DSL readable for a human.
    #[test]
    fn bare_word_is_unquoted() {
        assert_eq!(dsl_value("blue").as_deref(), Some("blue"));
        assert_eq!(dsl_value("drum-and-bass").as_deref(), Some("drum-and-bass"));
        assert_eq!(dsl_value("good vibes").as_deref(), Some(r#""good vibes""#));
        assert_eq!(dsl_value("").as_deref(), Some(r#""""#));
    }

    // The tokenizer splits a whole line the daemon's way, so a rendered value
    // stays ONE argument among others.
    #[test]
    fn tokenize_splits_a_whole_line() {
        let line = format!("action enqueue query {} 3", dsl_value("good vibes").unwrap());
        assert_eq!(
            tokenize(&line),
            vec!["action", "enqueue", "query", "good vibes", "3"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }
}
