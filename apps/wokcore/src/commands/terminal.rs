pub(super) fn escape_single_line(value: &str) -> String {
    escape(value, false)
}

pub(super) fn escape_message_body(value: &str) -> String {
    escape(value, true)
}

fn escape(value: &str, message_body: bool) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' if message_body => escaped.push('\n'),
            '\t' if message_body => escaped.push('\t'),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            '\0' => escaped.push_str("\\0"),
            '\u{1b}' => escaped.push_str("\\x1b"),
            '\u{7f}' => escaped.push_str("\\x7f"),
            character if must_escape(character) => {
                escaped.push_str(&format!("\\u{{{:04x}}}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn must_escape(character: char) -> bool {
    matches!(
        character as u32,
        0x0001..=0x001f
            | 0x0080..=0x009f
            | 0x202a..=0x202e
            | 0x2066..=0x2069
    )
}

#[cfg(test)]
mod tests {
    use super::{escape_message_body, escape_single_line};

    #[test]
    fn terminal_text_escapes_controls_and_bidi_without_damaging_unicode() {
        let hostile = "中🙂\n\t\r\0\u{1b}\u{7f}\u{0085}\u{202e}";
        assert_eq!(
            escape_single_line(hostile),
            "中🙂\\n\\t\\r\\0\\x1b\\x7f\\u{0085}\\u{202e}"
        );
        assert_eq!(
            escape_message_body(hostile),
            "中🙂\n\t\\r\\0\\x1b\\x7f\\u{0085}\\u{202e}"
        );
    }
}
