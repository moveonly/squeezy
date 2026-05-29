use super::*;

#[test]
fn empty_key_passes_through_unchanged() {
    assert_eq!(clamp_prompt_cache_key(""), "");
}

#[test]
fn short_ascii_key_passes_through_unchanged() {
    let key = "squeezy::session-1";
    assert_eq!(clamp_prompt_cache_key(key), key);
}

#[test]
fn exactly_sixty_four_ascii_codepoints_pass_through_unchanged() {
    let key: String = "a".repeat(OPENAI_PROMPT_CACHE_KEY_MAX_CODEPOINTS);
    assert_eq!(key.chars().count(), 64);
    assert_eq!(key.len(), 64);
    assert_eq!(clamp_prompt_cache_key(&key), key);
}

#[test]
fn one_hundred_ascii_codepoints_clamp_to_sixty_four_chars() {
    // F11 reproducer: a 100-character session id (e.g. a UUID + namespace
    // prefix) must clamp to exactly 64 codepoints so OpenAI's silent
    // validation does not drop the field on the server side.
    let key: String = "a".repeat(100);
    let clamped = clamp_prompt_cache_key(&key);
    assert_eq!(clamped.chars().count(), 64);
    assert_eq!(clamped, "a".repeat(64));
}

#[test]
fn sixty_five_ascii_codepoints_clamp_to_sixty_four_chars() {
    let key: String = "a".repeat(65);
    let clamped = clamp_prompt_cache_key(&key);
    assert_eq!(clamped.chars().count(), 64);
    assert_eq!(clamped, "a".repeat(64));
}

#[test]
fn sixty_four_multibyte_codepoints_pass_through_unchanged_despite_byte_length() {
    // Multibyte regression guard: clamp must count codepoints, not bytes.
    // A 64-codepoint string of 2-byte characters is 128 bytes — well over
    // 64 bytes — but only 64 codepoints, so it must survive unchanged.
    let key: String = "α".repeat(OPENAI_PROMPT_CACHE_KEY_MAX_CODEPOINTS);
    assert_eq!(key.chars().count(), 64);
    assert_eq!(key.len(), 128, "two-byte UTF-8 sanity check");
    assert_eq!(clamp_prompt_cache_key(&key), key);
}

#[test]
fn sixty_five_multibyte_codepoints_clamp_at_codepoint_boundary() {
    // 65 two-byte codepoints (130 bytes) must clamp to 64 codepoints
    // (128 bytes) on a clean codepoint boundary — never mid-character.
    let key: String = "α".repeat(65);
    let clamped = clamp_prompt_cache_key(&key);
    assert_eq!(clamped.chars().count(), 64);
    assert_eq!(clamped.len(), 128);
    assert!(clamped.is_char_boundary(clamped.len()));
    assert_eq!(clamped, "α".repeat(64));
}

#[test]
fn mixed_ascii_and_multibyte_codepoints_clamp_at_codepoint_boundary() {
    // Mixed ASCII + multibyte: total 70 codepoints (60 ASCII + 10 α),
    // total bytes 60 + 20 = 80. Clamp keeps the first 64 codepoints —
    // 60 ASCII + 4 α — i.e. byte length 60 + 8 = 68.
    let mut key = "a".repeat(60);
    key.push_str(&"α".repeat(10));
    assert_eq!(key.chars().count(), 70);
    let clamped = clamp_prompt_cache_key(&key);
    assert_eq!(clamped.chars().count(), 64);
    assert_eq!(clamped.len(), 60 + 4 * 2);
    let expected = format!("{}{}", "a".repeat(60), "α".repeat(4));
    assert_eq!(clamped, expected);
}

#[test]
fn four_byte_codepoints_clamp_at_codepoint_boundary() {
    // Four-byte UTF-8 (astral plane, e.g. 𝄞 U+1D11E) must still count as
    // one codepoint per character. 65 such characters = 65 codepoints
    // = 260 bytes; clamp keeps 64 codepoints = 256 bytes.
    let key: String = "𝄞".repeat(65);
    assert_eq!(key.chars().count(), 65);
    assert_eq!(key.len(), 260);
    let clamped = clamp_prompt_cache_key(&key);
    assert_eq!(clamped.chars().count(), 64);
    assert_eq!(clamped.len(), 256);
    assert!(clamped.is_char_boundary(clamped.len()));
}
