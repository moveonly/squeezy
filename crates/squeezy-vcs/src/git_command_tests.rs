use super::*;

#[test]
fn add_ignored_advisory_parser_accepts_only_advisory_stderr() {
    assert!(is_add_ignored_advisory_only(
        "The following paths are ignored by one of your .gitignore files:\n\
         .squeezy\n\
         hint: Use -f if you really want to add them.\n\
         hint: Turn this message off by running\n\
         hint: \"git config advice.addIgnoredFile false\"\n"
    ));
    assert!(is_add_ignored_advisory_only(
        "The following paths are ignored by one of your .gitignore files:\n.squeezy\n"
    ));
    assert!(!is_add_ignored_advisory_only(
        "fatal: pathspec 'missing' did not match any files\n"
    ));
    assert!(!is_add_ignored_advisory_only(""));
    assert!(!is_add_ignored_advisory_only(
        "The following paths are ignored by one of your .gitignore files:\n\
         .squeezy\n\
         fatal: unrelated index corruption\n"
    ));
}
