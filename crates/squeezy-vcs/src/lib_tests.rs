use super::*;

#[test]
fn parses_patch_hunks_as_zero_based_line_ranges() {
    let patch = "@@ -1,2 +1,3 @@\n-a\n+b\n+c\n@@ -10 +12,2 @@\n";
    let hunks = parse_patch_hunks(patch);
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[0].start_line, 0);
    assert_eq!(hunks[0].end_line, 2);
    assert_eq!(hunks[1].start_line, 11);
    assert_eq!(hunks[1].end_line, 12);
}

#[test]
fn parses_numstat_with_binary_counts() {
    let parsed = parse_numstat(b"2\t3\tsrc/lib.rs\0-\t-\timage.png\0");
    assert_eq!(parsed["src/lib.rs"].additions, 2);
    assert_eq!(parsed["src/lib.rs"].deletions, 3);
    assert!(parsed["image.png"].binary);
}
