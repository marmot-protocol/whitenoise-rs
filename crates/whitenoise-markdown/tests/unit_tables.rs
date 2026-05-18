use whitenoise_markdown::{Alignment, Block, Inline, TableCell, parse};

mod common;
use common::parse_blocks;

fn cell(text: &str) -> TableCell {
    TableCell {
        inlines: if text.is_empty() {
            vec![]
        } else {
            vec![Inline::Text(text.to_string())]
        },
    }
}

fn table(alignments: Vec<Alignment>, header: Vec<TableCell>, rows: Vec<Vec<TableCell>>) -> Block {
    Block::Table {
        alignments,
        header,
        rows,
    }
}

#[test]
fn table_simple() {
    let input = "| a | b |\n| - | - |\n| 1 | 2 |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None, Alignment::None],
            vec![cell("a"), cell("b")],
            vec![vec![cell("1"), cell("2")]],
        )]
    );
}

#[test]
fn table_alignments() {
    let input = "| a | b | c | d |\n| :- | :-: | -: | - |\n| 1 | 2 | 3 | 4 |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![
                Alignment::Left,
                Alignment::Center,
                Alignment::Right,
                Alignment::None
            ],
            vec![cell("a"), cell("b"), cell("c"), cell("d")],
            vec![vec![cell("1"), cell("2"), cell("3"), cell("4")]],
        )]
    );
}

#[test]
fn table_no_outer_pipes() {
    let input = "a | b\n- | -\n1 | 2";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None, Alignment::None],
            vec![cell("a"), cell("b")],
            vec![vec![cell("1"), cell("2")]],
        )]
    );
}

#[test]
fn table_escaped_pipe_in_cell() {
    let input = "| a | b |\n| - | - |\n| 1 \\| x | 2 |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None, Alignment::None],
            vec![cell("a"), cell("b")],
            vec![vec![cell("1 | x"), cell("2")]],
        )]
    );
}

#[test]
fn table_ragged_row_padded() {
    let input = "| a | b | c |\n| - | - | - |\n| 1 |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None, Alignment::None, Alignment::None],
            vec![cell("a"), cell("b"), cell("c")],
            vec![vec![cell("1"), cell(""), cell("")]],
        )]
    );
}

#[test]
fn table_ragged_row_truncated() {
    let input = "| a |\n| - |\n| 1 | 2 | 3 |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None],
            vec![cell("a")],
            vec![vec![cell("1")]],
        )]
    );
}

#[test]
fn table_closed_by_blank_then_paragraph() {
    let input = "| a |\n| - |\n| 1 |\n\nfoo";
    use common::paragraph;
    assert_eq!(
        parse_blocks(input),
        vec![
            table(
                vec![Alignment::None],
                vec![cell("a")],
                vec![vec![cell("1")]],
            ),
            paragraph("foo"),
        ]
    );
}

#[test]
fn table_closed_by_non_row_line() {
    // A line without `|` closes the table.
    let input = "| a |\n| - |\n| 1 |\nfoo";
    use common::paragraph;
    assert_eq!(
        parse_blocks(input),
        vec![
            table(
                vec![Alignment::None],
                vec![cell("a")],
                vec![vec![cell("1")]],
            ),
            paragraph("foo"),
        ]
    );
}

#[test]
fn table_delimiter_column_mismatch_is_paragraph() {
    // Header has 2 cells, delimiter has 3 → not a table.
    let input = "| a | b |\n| - | - | - |";
    let parsed = parse(input).blocks;
    // Should be a paragraph (no table).
    assert!(matches!(parsed.as_slice(), [Block::Paragraph { .. }]));
}

#[test]
fn table_cell_contains_emphasis() {
    let input = "| a |\n| - |\n| *b* |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None],
            vec![cell("a")],
            vec![vec![TableCell {
                inlines: vec![Inline::Emph(vec![Inline::Text("b".into())])]
            }]],
        )]
    );
}

#[test]
fn table_cells_preserve_non_ascii_utf8() {
    let input = "| 日本 | 語 |\n| - | - |\n| あ🦫 | い🦫 |";
    assert_eq!(
        parse_blocks(input),
        vec![table(
            vec![Alignment::None, Alignment::None],
            vec![cell("日本"), cell("語")],
            vec![vec![cell("あ🦫"), cell("い🦫")]],
        )]
    );
}

#[test]
fn table_cell_contains_nostr_mention() {
    use whitenoise_markdown::{NostrEntity, NostrHrp};
    let body = "xyq6ag2g4cd2y6h4r4ag2y3xeak0v6gxq46v9";
    let input = format!("| who |\n| - |\n| @npub1{body} |");
    let entity = NostrEntity {
        hrp: NostrHrp::Npub,
        bech32: format!("npub1{body}"),
    };
    assert_eq!(
        parse_blocks(&input),
        vec![table(
            vec![Alignment::None],
            vec![cell("who")],
            vec![vec![TableCell {
                inlines: vec![Inline::NostrMention(entity)]
            }]],
        )]
    );
}
