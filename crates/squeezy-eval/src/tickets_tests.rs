use super::*;

#[test]
fn slug_truncates_and_lowercases() {
    let slug = sanitize_slug("This is a Very Long, Wordy Title!!!");
    assert!(slug.len() <= 48);
    assert!(
        slug.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
}

#[test]
fn slug_falls_back_when_empty() {
    assert_eq!(sanitize_slug(""), "ticket");
    assert_eq!(sanitize_slug("!!!"), "ticket");
}

#[test]
fn renders_basic_markdown() {
    let ticket = TicketDraft {
        id: "x".into(),
        title: "Bug".into(),
        severity: "minor".into(),
        category: "ux".into(),
        summary: "summary".into(),
        repro: "repro".into(),
        evidence: vec![EvidencePointer {
            trace_event: Some(3),
            frame: None,
        }],
        suggested_fix: None,
    };
    let md = render_markdown(&ticket, None);
    assert!(md.contains("# [squeezy-eval] Bug"));
    assert!(md.contains("trace_event 3"));
    assert!(!md.contains("## Bundle"));

    let bundle_path = std::path::PathBuf::from("/tmp/bundle.tar.gz");
    let md_with_bundle = render_markdown(&ticket, Some(&bundle_path));
    assert!(md_with_bundle.contains("## Bundle"));
    assert!(md_with_bundle.contains("/tmp/bundle.tar.gz"));
}
