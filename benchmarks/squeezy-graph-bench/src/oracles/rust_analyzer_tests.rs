use std::path::Path;

use serde_json::json;

use crate::report::{LocationKey, LspPosition};

use super::{byte_to_lsp_position, parse_lsp_locations};

#[test]
fn byte_to_lsp_position_counts_utf16_characters() {
    let source = "a\néx\n";

    assert_eq!(
        byte_to_lsp_position(source, 4),
        LspPosition {
            line: 1,
            character: 1
        }
    );
}

#[test]
fn parse_lsp_locations_relativizes_locations_and_location_links() {
    let value = json!([
        {
            "uri": "file:///repo/src/lib.rs",
            "range": {"start": {"line": 2, "character": 4}}
        },
        {
            "targetUri": "file:///repo/src/main.rs",
            "targetSelectionRange": {"start": {"line": 5, "character": 8}}
        }
    ]);

    let locations = parse_lsp_locations(&value, Path::new("/repo")).unwrap();

    assert_eq!(
        locations,
        vec![
            LocationKey {
                file: "src/lib.rs".to_string(),
                line: 2,
                character: 4
            },
            LocationKey {
                file: "src/main.rs".to_string(),
                line: 5,
                character: 8
            }
        ]
    );
}
