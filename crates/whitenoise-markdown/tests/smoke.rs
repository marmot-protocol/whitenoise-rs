use whitenoise_markdown::parse;

#[test]
fn empty_input_yields_empty_document() {
    let doc = parse("");
    assert!(doc.blocks.is_empty());
}
