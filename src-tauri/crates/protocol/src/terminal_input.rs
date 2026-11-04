//! Attention classification at the complete input-frame boundary used by xterm/native compose.
//! This is not a permission filter or a streaming VT parser.

/// Whether a complete input frame contains inserted text. Escape-prefixed traffic is
/// conservatively ignored, except a complete bracketed paste whose body contains text.
pub fn is_text_frame(data: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(data) else {
        return false;
    };
    let body = if text.starts_with('\u{1b}') {
        let Some(body) = text
            .strip_prefix("\u{1b}[200~")
            .and_then(|s| s.strip_suffix("\u{1b}[201~"))
        else {
            return false;
        };
        body
    } else {
        text
    };
    // Embedded escape/control protocols are not ordinary text frames either. Line breaks
    // and compose's trailing CR are allowed alongside actual printable text.
    if body.contains('\u{1b}') || body.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)) {
        return false;
    }
    body.chars().any(|c| !c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_text_and_deliberate_paste() {
        for s in [
            "hello",
            " ",
            "ö🦀",
            "hello\r",
            "\u{1b}[200~a\nb ö🦀\u{1b}[201~",
        ] {
            assert!(is_text_frame(s.as_bytes()), "{s:?}");
        }
    }

    #[test]
    fn controls_replies_navigation_and_incomplete_frames_are_not_text() {
        for s in [
            "",
            "\r\n\t\u{7f}\u{3}",
            "\u{1b}",
            "\u{1b}[A",
            "\u{1b}OP",
            "\u{1b}[1;2R",
            "\u{1b}[?1;2c",
            "\u{1b}[I",
            "\u{1b}[O",
            "\u{1b}[<0;20;30M",
            "\u{1b}]10;rgb:ffff/ffff/ffff\u{7}",
            "\u{1b}P1$r0m\u{1b}\\",
            "\u{1b}[200~text",
            "\u{1b}[200~\u{1b}[201~",
            "\u{1b}[200~\r\t\u{1b}[201~",
            "text\u{1b}[A",
            "\u{9b}1;2R",
        ] {
            assert!(!is_text_frame(s.as_bytes()), "{s:?}");
        }
        assert!(!is_text_frame(&[0xff]));
    }
}
