use super::*;

fn doc<'a>(signature: &'a str, docs: &'a str, attributes: &'a str) -> BM25Doc<'a> {
    BM25Doc {
        signature,
        docs,
        attributes,
    }
}

#[test]
fn bm25_rerank_outranks_substring_on_multiword_query() {
    let corpus = vec![
        doc("fn rust_file_writer(path: &Path)", "", ""),
        doc("fn parse_rust_file(path: &Path)", "", ""),
        doc("fn parse_python_file(path: &Path)", "", ""),
    ];
    let results = bm25_rerank(&corpus, "parse rust file", 3);
    assert!(!results.is_empty(), "expected at least one result");
    assert_eq!(
        corpus[results[0].0].signature,
        "fn parse_rust_file(path: &Path)"
    );
}

#[test]
fn bm25_rerank_uses_docs_and_attributes() {
    let corpus = vec![
        doc("fn handler_one()", "", ""),
        doc(
            "fn unrelated()",
            "parses a rust source file into AST",
            "#[doc] #[parser]",
        ),
    ];
    let results = bm25_rerank(&corpus, "parse rust file", 2);
    assert!(!results.is_empty());
    assert_eq!(corpus[results[0].0].signature, "fn unrelated()");
}

#[test]
fn empty_input_returns_empty() {
    let results = bm25_rerank(&[], "anything", 5);
    assert!(results.is_empty());
    let corpus = vec![doc("a", "b", "c")];
    let results = bm25_rerank(&corpus, "anything", 0);
    assert!(results.is_empty());
}
