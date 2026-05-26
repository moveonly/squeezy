use super::*;

#[test]
fn match_slash_command_prefix_returns_command_length() {
    assert_eq!(match_slash_command_prefix("/help"), Some(5));
    assert_eq!(
        match_slash_command_prefix("/help changing the model"),
        Some(5)
    );
}

#[test]
fn match_slash_command_prefix_prefers_longest_match() {
    // `/job-cancel foo` must resolve to `/job-cancel`, not `/job`.
    assert_eq!(
        match_slash_command_prefix("/job-cancel abc"),
        Some("/job-cancel".len())
    );
}

#[test]
fn match_slash_command_prefix_requires_word_boundary() {
    // `/helpme` is not `/help`.
    assert_eq!(match_slash_command_prefix("/helpme"), None);
    // `/config-foo` is not `/config`.
    assert_eq!(match_slash_command_prefix("/config-foo"), None);
}

#[test]
fn match_slash_command_prefix_rejects_unknown_or_non_slash() {
    assert_eq!(match_slash_command_prefix("/notacommand"), None);
    assert_eq!(match_slash_command_prefix("help"), None);
    assert_eq!(match_slash_command_prefix(""), None);
}
