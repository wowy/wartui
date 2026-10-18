//! Compile-time sizes taken from the environment.
//!
//! A buffer's size is a const generic, so it is fixed when the firmware builds. The
//! firmware reads `option_env!("WARTUI_*")` and hands the text here, which keeps one
//! parser for both chips and lets a size change cost a rebuild rather than an edit.
//! A value that is not a number panics during const evaluation, so a typo is a build
//! error and never a silently default-sized buffer.

/// The size an environment variable names, or `default` when it is unset.
///
/// Digits parse, and `_` separators are skipped so `262_144` reads as written.
///
/// # Panics
/// In const evaluation, on anything else: empty text, a sign, a unit suffix.
#[must_use]
pub const fn env_usize(value: Option<&str>, default: usize) -> usize {
    let Some(text) = value else {
        return default;
    };
    let bytes = text.as_bytes();
    let mut parsed = 0usize;
    let mut digits = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                parsed = parsed * 10 + (bytes[i] - b'0') as usize;
                digits = true;
            }
            b'_' => {}
            _ => panic!("a WARTUI_* size must be a decimal number"),
        }
        i += 1;
    }
    assert!(digits, "a WARTUI_* size must be a decimal number");
    parsed
}

#[cfg(test)]
mod tests {
    use super::env_usize;

    #[test]
    fn config_env_usize_returns_default_when_unset() {
        assert_eq!(env_usize(None, 4096), 4096);
    }

    #[test]
    fn config_env_usize_parses_digits_and_underscores() {
        assert_eq!(env_usize(Some("262_144"), 1), 262_144);
    }

    #[test]
    #[should_panic(expected = "a WARTUI_* size must be a decimal number")]
    fn config_env_usize_panics_when_text_is_not_a_number() {
        let _ = env_usize(Some("64k"), 1);
    }
}
