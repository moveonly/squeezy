use std::{
    cell::{Cell, RefCell},
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

fn ok_stdin<'a>(
    content: &'static str,
    calls: &'a Cell<usize>,
) -> impl FnMut() -> Result<String, SqueezyError> + 'a {
    move || {
        calls.set(calls.get() + 1);
        Ok(content.to_string())
    }
}

fn fail_stdin() -> impl FnMut() -> Result<String, SqueezyError> {
    || panic!("stdin reader must not be invoked when stdin is a TTY")
}

fn ok_file(
    files: &RefCell<Vec<PathBuf>>,
) -> impl FnMut(&std::path::Path) -> Result<String, SqueezyError> + '_ {
    move |path| {
        files.borrow_mut().push(path.to_path_buf());
        Ok(format!("FILE({})", path.display()))
    }
}

fn fail_file() -> impl FnMut(&std::path::Path) -> Result<String, SqueezyError> {
    |path| panic!("file reader must not be invoked, got {}", path.display())
}

fn temp_dir(name: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = env::temp_dir().join(format!("squeezy-print-mode-{name}-{suffix}"));
    fs::create_dir_all(&path).expect("temp dir");
    path
}

#[test]
fn returns_empty_when_no_inputs_and_tty_stdin() {
    let prompts = resolve_prompt_inputs(&[], true, fail_stdin(), fail_file()).expect("resolve");
    assert!(prompts.is_empty());
}

#[test]
fn passes_plain_values_through_in_order() {
    let prompts = resolve_prompt_inputs(
        &["first".to_string(), "second".to_string()],
        true,
        fail_stdin(),
        fail_file(),
    )
    .expect("resolve");
    assert_eq!(prompts, vec!["first".to_string(), "second".to_string()]);
}

#[test]
fn at_mention_uses_file_reader_with_unprefixed_path() {
    let touched = RefCell::new(Vec::new());
    let prompts = resolve_prompt_inputs(
        &["@notes/todo.txt".to_string()],
        true,
        fail_stdin(),
        ok_file(&touched),
    )
    .expect("resolve");
    assert_eq!(prompts, vec!["FILE(notes/todo.txt)".to_string()]);
    let touched = touched.into_inner();
    assert_eq!(touched.len(), 1);
    assert_eq!(touched[0].to_string_lossy(), "notes/todo.txt");
}

#[test]
fn empty_at_mention_errors() {
    let err = resolve_prompt_inputs(&["@".to_string()], true, fail_stdin(), fail_file())
        .expect_err("`@` alone is not a valid path");
    assert!(format!("{err}").contains("--prompt @"), "got: {err}");
}

#[test]
fn dash_value_with_tty_stdin_errors() {
    let err = resolve_prompt_inputs(&["-".to_string()], true, fail_stdin(), fail_file())
        .expect_err("`-` requires piped stdin");
    let message = format!("{err}");
    assert!(message.contains("stdin is a TTY"), "got: {message}");
}

#[test]
fn dash_value_with_piped_stdin_consumes_once() {
    let calls = Cell::new(0usize);
    let prompts = resolve_prompt_inputs(
        &["-".to_string(), "follow-up".to_string(), "-".to_string()],
        false,
        ok_stdin("piped body", &calls),
        fail_file(),
    )
    .expect("resolve");
    assert_eq!(
        prompts,
        vec![
            "piped body".to_string(),
            "follow-up".to_string(),
            "piped body".to_string(),
        ]
    );
    assert_eq!(calls.get(), 1, "stdin must be read exactly once");
}

#[test]
fn piped_stdin_alone_becomes_sole_prompt() {
    let calls = Cell::new(0usize);
    let prompts = resolve_prompt_inputs(&[], false, ok_stdin("read me", &calls), fail_file())
        .expect("resolve");
    assert_eq!(prompts, vec!["read me".to_string()]);
    assert_eq!(calls.get(), 1);
}

#[test]
fn piped_stdin_prepends_to_first_prompt_with_blank_line() {
    let calls = Cell::new(0usize);
    let prompts = resolve_prompt_inputs(
        &["explain this".to_string(), "and again".to_string()],
        false,
        ok_stdin("context", &calls),
        fail_file(),
    )
    .expect("resolve");
    assert_eq!(
        prompts,
        vec![
            "context\n\nexplain this".to_string(),
            "and again".to_string()
        ]
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn explicit_dash_suppresses_auto_prepend() {
    let calls = Cell::new(0usize);
    let prompts = resolve_prompt_inputs(
        &["-".to_string(), "talk".to_string()],
        false,
        ok_stdin("stdin only", &calls),
        fail_file(),
    )
    .expect("resolve");
    assert_eq!(prompts, vec!["stdin only".to_string(), "talk".to_string()]);
    assert_eq!(calls.get(), 1, "stdin still consumed exactly once");
}

#[test]
fn empty_piped_stdin_is_ignored_when_combined_with_prompt() {
    let calls = Cell::new(0usize);
    let prompts = resolve_prompt_inputs(
        &["explain".to_string()],
        false,
        ok_stdin("   \n", &calls),
        fail_file(),
    )
    .expect("resolve");
    assert_eq!(prompts, vec!["explain".to_string()]);
    assert_eq!(calls.get(), 1);
}

#[test]
fn read_prompt_file_wraps_utf8_text_with_path_envelope() {
    let dir = temp_dir("text");
    let path = dir.join("note.md");
    fs::write(&path, "hello\nworld\n").expect("write");

    let rendered = read_prompt_file(&path).expect("read text");
    assert!(rendered.starts_with(&format!("<file path=\"{}\">", path.display())));
    assert!(rendered.contains("hello\nworld\n"));
    assert!(rendered.trim_end().ends_with("</file>"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn read_prompt_file_rejects_image_extensions() {
    let dir = temp_dir("image");
    let path = dir.join("screenshot.png");
    fs::write(&path, b"fake-png").expect("write");

    let err = read_prompt_file(&path).expect_err("PNG must be rejected until multimodal lands");
    let message = format!("{err}");
    assert!(message.contains("image attachments"), "got: {message}");
    assert!(message.contains("screenshot.png"), "got: {message}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn read_prompt_file_reports_missing_path() {
    let dir = temp_dir("missing");
    let path = dir.join("absent.txt");

    let err = read_prompt_file(&path).expect_err("missing file must surface");
    let message = format!("{err}");
    assert!(message.contains("failed to read file"), "got: {message}");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn is_image_extension_covers_pi_supported_image_types() {
    for ext in [
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "tif", "tiff", "ico", "heic", "heif", "avif",
        "svg",
    ] {
        assert!(is_image_extension(ext), "expected {ext} to be image");
    }
    for ext in ["md", "txt", "rs", "toml", "json", "py", "ts", ""] {
        assert!(!is_image_extension(ext), "expected {ext} to be text");
    }
}

#[test]
fn classify_prompt_strips_double_bang_prefix_and_sets_exclude_flag() {
    let classified = classify_prompt("!!ls ~/.ssh".to_string());
    assert_eq!(
        classified,
        PromptInput {
            content: "ls ~/.ssh".to_string(),
            exclude_from_context: true,
        }
    );
}

#[test]
fn classify_prompt_leaves_plain_text_untouched() {
    let classified = classify_prompt("explain README.md".to_string());
    assert_eq!(
        classified,
        PromptInput {
            content: "explain README.md".to_string(),
            exclude_from_context: false,
        }
    );
}

#[test]
fn classify_prompt_treats_single_bang_as_normal_content() {
    // Pi's single-bang `!cmd` escape (LLM sees output) is not yet a
    // print-mode shortcut. Leaving a single leading `!` in place keeps
    // legitimate prompt bodies like `"!important: rerun the migration"`
    // intact while reserving `!!` for the exclude semantic.
    let classified = classify_prompt("!important note".to_string());
    assert_eq!(
        classified,
        PromptInput {
            content: "!important note".to_string(),
            exclude_from_context: false,
        }
    );
}

#[test]
fn classify_prompt_keeps_whitespace_before_bang_bang_as_content() {
    // Detection is strict prefix-only so callers can't accidentally hide
    // the exclude semantic behind a leading newline or space — the
    // double-bang has to be the literal first two bytes.
    let classified = classify_prompt(" !!ls".to_string());
    assert_eq!(
        classified,
        PromptInput {
            content: " !!ls".to_string(),
            exclude_from_context: false,
        }
    );
}

#[test]
fn classify_prompt_handles_empty_command_body_after_prefix() {
    // `!!` alone is meaningless but should not panic; the empty body is
    // passed through with the flag so the pump can surface a clear
    // diagnostic instead of dispatching an empty turn.
    let classified = classify_prompt("!!".to_string());
    assert_eq!(
        classified,
        PromptInput {
            content: String::new(),
            exclude_from_context: true,
        }
    );
}
