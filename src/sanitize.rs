//! Escaping untrusted text on its way to a terminal.
//!
//! Almost everything this CLI prints came from somewhere else: an alert summary
//! typed by whoever triggered the alert, a service name from the API, an error
//! message from a gateway, a description out of the OpenAPI document. A
//! terminal reads its input as a command stream, so any of those can carry an
//! escape sequence — and then the bytes stop being text.
//!
//! The realistic ones, in rough order of nastiness:
//!
//! - **OSC 52** (`ESC ] 52 ; c ; <base64> BEL`) writes the terminal's clipboard.
//!   An alert summary can put a command into the user's paste buffer.
//! - **CSI** sequences move the cursor, clear the screen, or set a colour that
//!   never gets reset — enough to overwrite the line above and make a refusal
//!   read as a success.
//! - **A bare newline or carriage return** forges a second line of output.
//!   `\rError: none` after a real error is the whole attack.
//! - **Bidi overrides** (U+202E and friends) reorder what the eye sees without
//!   changing what the bytes say, so a table cell can display one hostname and
//!   contain another.
//! - **C1 control codes** — U+0085 through U+009F — reach the same CSI/OSC
//!   dispatch as `ESC [` and `ESC ]` on terminals decoding UTF-8, so escaping
//!   `ESC` alone is not enough.
//!
//! What this module does *not* cover is machine-readable output. `-o json`,
//! `-o ndjson` and `-o raw` are contracts with a program, and serde already
//! escapes control characters inside JSON strings; rewriting the payload there
//! would corrupt data the caller asked for verbatim.
//!
//! Escapes are visible rather than silent (`\n`, `\u{001B}`) so that what was
//! removed is still legible — a summary that gets quietly shortened looks like
//! a truncation bug, and a hostname that quietly loses a character looks like
//! the hostname.

/// Unicode's `Bidi_Control` property, in full.
///
/// The embedding and override codes (U+202A–U+202E) and the isolates
/// (U+2066–U+2069) are the ones that reorder a run outright. The three marks —
/// ALM, LRM and RLM — are subtler but not harmless: they carry a strong
/// direction of their own, so they can flip the display order of neutral
/// characters such as `/`, `.`, `:` and `-` around them. That is enough to make
/// a hostname or a path read as something it is not, which is the same attack
/// with fewer bytes.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Whether [`terminal_text`] would rewrite this character.
pub fn is_unsafe_for_terminal(c: char) -> bool {
    // `char::is_control` is exactly the Cc category: C0 (U+0000–U+001F), DEL
    // (U+007F) and C1 (U+0080–U+009F).
    c.is_control() || is_bidi_control(c)
}

/// Untrusted text, rendered so a terminal treats every byte of it as text.
///
/// Idempotent in practice on ordinary content: a string with nothing to escape
/// is returned unchanged, and the replacements are all ASCII.
pub fn terminal_text(s: &str) -> String {
    if !s.chars().any(is_unsafe_for_terminal) {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            // The three that have a spelling everyone already reads.
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ if is_unsafe_for_terminal(c) => {
                out.push_str(&format!("\\u{{{:04X}}}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

/// [`terminal_text`] for an owned string, avoiding the copy when there is
/// nothing to escape.
pub fn terminal_string(s: String) -> String {
    if s.chars().any(is_unsafe_for_terminal) {
        terminal_text(&s)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_is_returned_unchanged() {
        for s in [
            "Disk usage above 90% on web-01",
            "",
            "id=42 / priority=HIGH",
            // Non-ASCII text is text, not an escape.
            "Störung im Rechenzentrum München",
            "重大なアラート",
            "🚨 pager fired",
            // A backslash-u that is already literal must not be re-mangled.
            "path\\to\\thing",
        ] {
            assert_eq!(terminal_text(s), s, "{s:?} should pass through");
        }
    }

    #[test]
    fn an_ansi_colour_sequence_loses_its_escape() {
        let out = terminal_text("\u{1b}[31mRED\u{1b}[0m");
        assert!(!out.contains('\u{1b}'), "{out}");
        assert_eq!(out, "\\u{001B}[31mRED\\u{001B}[0m");
    }

    /// The one that reaches outside the terminal: OSC 52 sets the clipboard.
    #[test]
    fn an_osc_52_clipboard_write_is_defused() {
        let payload = "\u{1b}]52;c;cm0gLXJmIC8=\u{7}";
        let out = terminal_text(payload);
        assert!(!out.contains('\u{1b}'));
        assert!(!out.contains('\u{7}'), "BEL terminator survived: {out:?}");
        assert!(out.contains("\\u{0007}"));
    }

    /// C1 introducers dispatch the same as `ESC [` / `ESC ]`, so escaping only
    /// U+001B leaves the attack intact.
    #[test]
    fn c1_control_introducers_are_escaped_too() {
        // U+009B is CSI, U+009D is OSC.
        let out = terminal_text("a\u{9b}31mb\u{9d}52;c;x");
        assert!(!out.contains('\u{9b}'), "{out:?}");
        assert!(!out.contains('\u{9d}'), "{out:?}");
        assert!(out.contains("\\u{009B}"));
        assert!(out.contains("\\u{009D}"));
    }

    #[test]
    fn embedded_newlines_cannot_forge_a_second_line() {
        assert_eq!(
            terminal_text("ok\nError: nothing wrong"),
            "ok\\nError: nothing wrong"
        );
        assert_eq!(
            terminal_text("real error\rall fine"),
            "real error\\rall fine"
        );
        assert_eq!(terminal_text("a\tb"), "a\\tb");
        assert_eq!(terminal_text("a\r\nb"), "a\\r\\nb");
    }

    #[test]
    fn bidi_overrides_cannot_reorder_what_is_displayed() {
        // Reads as "api.ilert.com" while containing "moc.live.
        let spoof = "\u{202E}moc.elpmaxe.live\u{202C}";
        let out = terminal_text(spoof);
        assert!(!out.chars().any(is_bidi_control), "{out}");
        assert!(out.contains("\\u{202E}"));
        assert!(out.contains("\\u{202C}"));

        for c in ['\u{202A}', '\u{202B}', '\u{202D}', '\u{2066}', '\u{2069}'] {
            let out = terminal_text(&c.to_string());
            assert!(out.starts_with("\\u{"), "{c:?} was not escaped: {out}");
        }
    }

    /// The whole `Bidi_Control` property, not just the loud half. The marks are
    /// invisible and carry a strong direction, so they reorder the neutral
    /// punctuation around them — enough to disguise a host or a path.
    #[test]
    fn the_directional_marks_are_covered_as_well() {
        for c in ['\u{061C}', '\u{200E}', '\u{200F}'] {
            let out = terminal_text(&c.to_string());
            assert!(out.starts_with("\\u{"), "{c:?} was not escaped: {out}");
            assert!(is_unsafe_for_terminal(c));
        }
        // A URL that displays right-to-left around the separators.
        let spoof = "api.ilert.com\u{200F}/\u{200F}evil.example";
        assert!(!terminal_text(spoof).chars().any(is_bidi_control));
    }

    /// Zero-width characters that are *not* bidi controls stay: they are joiners
    /// and non-joiners that ordinary scripts need in order to render at all.
    #[test]
    fn non_bidi_invisibles_are_not_our_business() {
        for s in ["\u{200B}", "\u{200C}", "\u{200D}", "\u{FEFF}"] {
            assert_eq!(terminal_text(s), s);
        }
    }

    #[test]
    fn del_and_nul_are_escaped() {
        assert_eq!(terminal_text("a\u{7f}b"), "a\\u{007F}b");
        assert_eq!(terminal_text("a\0b"), "a\\u{0000}b");
    }

    #[test]
    fn escaping_is_idempotent() {
        let once = terminal_text("\u{1b}[2Ja\nb");
        assert_eq!(terminal_text(&once), once);
    }

    #[test]
    fn the_owned_form_agrees_with_the_borrowed_one() {
        for s in ["plain", "\u{1b}[31m", "a\nb"] {
            assert_eq!(terminal_string(s.to_string()), terminal_text(s));
        }
    }
}
