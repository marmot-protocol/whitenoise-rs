//! Byte-level scanning helpers shared by both passes.
//!
//! Strictly hand-coded ASCII predicates and line slicing; no regex, no
//! `once_cell`, no allocation outside what callers explicitly request.

/// Iterator over the input split into logical lines, with `\r\n` and bare
/// `\r` normalized to `\n`. Each yielded `&str` excludes the line ending.
///
/// The iterator yields one item per line; if the input ends without a
/// trailing newline, the final partial line is still yielded.
pub(crate) fn lines(input: &str) -> Lines<'_> {
    Lines { rest: input }
}

pub(crate) struct Lines<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.rest.is_empty() {
            return None;
        }
        let bytes = self.rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    let line = &self.rest[..i];
                    self.rest = &self.rest[i + 1..];
                    return Some(line);
                }
                b'\r' => {
                    let line = &self.rest[..i];
                    let mut step = 1;
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                        step = 2;
                    }
                    self.rest = &self.rest[i + step..];
                    return Some(line);
                }
                _ => i += 1,
            }
        }
        let line = self.rest;
        self.rest = "";
        Some(line)
    }
}

/// Measure leading indentation as (column count, byte offset).
///
/// Tabs advance to the next multiple of 4 columns. Stops at the first byte
/// that is neither space nor tab.
pub(crate) fn measure_indent(line: &[u8]) -> (usize, usize) {
    let mut col = 0;
    let mut off = 0;
    while off < line.len() {
        match line[off] {
            b' ' => {
                col += 1;
                off += 1;
            }
            b'\t' => {
                col += 4 - (col % 4);
                off += 1;
            }
            _ => break,
        }
    }
    (col, off)
}

/// True if every byte of `line` is ASCII space or tab (or empty).
pub(crate) fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|&b| b == b' ' || b == b'\t')
}
