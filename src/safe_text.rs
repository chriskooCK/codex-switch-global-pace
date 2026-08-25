use std::borrow::Cow;

/// Remove terminal control characters from untrusted display text while
/// preserving ordinary Unicode exactly. JSON serializers should keep using the
/// original value; this helper belongs only at human-readable render/storage
/// boundaries.
pub(crate) fn terminal_text(value: &str) -> Cow<'_, str> {
    if value.chars().any(char::is_control) {
        Cow::Owned(value.chars().filter(|ch| !ch.is_control()).collect())
    } else {
        Cow::Borrowed(value)
    }
}

/// Storage form for text that can later be replayed to a terminal. The caller
/// supplies the domain-specific retention limit; sanitizing and bounding stay
/// one operation so no path can accidentally persist controls first.
pub(crate) fn bounded_terminal_text(value: &str, max_chars: usize) -> String {
    terminal_text(value).chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_text_removes_c0_c1_and_escape_but_preserves_unicode() {
        assert_eq!(
            terminal_text("한글\u{1b}]52;clipboard\u{7}\n🙂"),
            "한글]52;clipboard🙂"
        );
        assert_eq!(terminal_text("Workspace 팀"), "Workspace 팀");
    }

    #[test]
    fn bounded_terminal_text_applies_the_limit_after_sanitizing() {
        assert_eq!(bounded_terminal_text("a\u{1b}bcdef", 4), "abcd");
    }
}
