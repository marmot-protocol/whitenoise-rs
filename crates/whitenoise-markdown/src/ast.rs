//! AST node definitions.
//!
//! Two enums (`Block`, `Inline`) plus a thin document wrapper. Owned `String`
//! everywhere for v1.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    ThematicBreak,
    CodeBlock {
        kind: CodeBlockKind,
        info: String,
        content: String,
    },
    BlockQuote {
        blocks: Vec<Block>,
    },
    List {
        kind: ListKind,
        tight: bool,
        items: Vec<ListItem>,
    },
    Table {
        alignments: Vec<Alignment>,
        header: Vec<TableCell>,
        rows: Vec<Vec<TableCell>>,
    },
    MathBlock {
        content: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CodeBlockKind {
    Indented,
    Fenced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListKind {
    Bullet { marker: u8 },
    Ordered { start: u32, delimiter: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    pub blocks: Vec<Block>,
    pub checked: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableCell {
    pub inlines: Vec<Inline>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Inline {
    Text(String),
    SoftBreak,
    HardBreak,
    Code(String),
    Emph(Vec<Inline>),
    Strong(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link {
        dest: String,
        title: Option<String>,
        children: Vec<Inline>,
    },
    Image {
        dest: String,
        title: Option<String>,
        alt: Vec<Inline>,
    },
    Autolink {
        url: String,
        kind: AutolinkKind,
    },
    Math(String),
    NostrMention(NostrEntity),
    NostrUri(NostrEntity),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutolinkKind {
    Uri,
    Email,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NostrEntity {
    pub hrp: NostrHrp,
    pub bech32: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NostrHrp {
    Npub,
    Note,
    Nevent,
    Nprofile,
    Naddr,
    Nrelay,
}
