//! Golden-file tests.
//!
//! Each fixture is a pair of files in `tests/golden/`:
//!
//! - `<name>.md`   — source Markdown
//! - `<name>.json` — `serde_json::to_string_pretty(&Document)` of the parsed
//!   AST, with a trailing newline.
//!
//! The test parses the `.md`, pretty-prints the AST to JSON, and asserts
//! byte-exact equality with the `.json`. A regression in the parser surfaces
//! as a structural diff.
//!
//! ## Regenerating fixtures
//!
//! To add a new pair, drop a `<name>.md` next to its siblings, add a
//! `#[test]` calling `assert_golden("<name>")`, then run:
//!
//! ```text
//! WHITENOISE_MD_UPDATE_GOLDEN=1 cargo test -p whitenoise-markdown --test golden
//! ```
//!
//! This rewrites every `.json` from the current parser output. Inspect the
//! diff, commit if intentional. Re-run without the env var to confirm the
//! goldens lock in.

use std::fs;
use std::path::PathBuf;

use whitenoise_markdown::parse;

const UPDATE_ENV: &str = "WHITENOISE_MD_UPDATE_GOLDEN";

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn assert_golden(name: &str) {
    let dir = golden_dir();
    let md_path = dir.join(format!("{name}.md"));
    let json_path = dir.join(format!("{name}.json"));

    let md =
        fs::read_to_string(&md_path).unwrap_or_else(|e| panic!("read {}: {e}", md_path.display()));
    let doc = parse(&md);
    let mut actual = serde_json::to_string_pretty(&doc).expect("serialize");
    actual.push('\n');

    let update = std::env::var(UPDATE_ENV).is_ok_and(|v| v == "1");
    if update {
        fs::write(&json_path, &actual)
            .unwrap_or_else(|e| panic!("write {}: {e}", json_path.display()));
        return;
    }

    let expected = fs::read_to_string(&json_path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e}\nhint: run `{UPDATE_ENV}=1 cargo test -p whitenoise-markdown --test golden` to create the fixture",
            json_path.display()
        )
    });

    if expected == actual {
        return;
    }

    // Report the first differing line to keep failures actionable without
    // dumping the whole AST.
    let mut first_diff: Option<(usize, String, String)> = None;
    for (i, (e, a)) in expected.lines().zip(actual.lines()).enumerate() {
        if e != a {
            first_diff = Some((i + 1, e.to_string(), a.to_string()));
            break;
        }
    }
    let (line, e_line, a_line) = first_diff.unwrap_or_else(|| {
        // Length-only differences (e.g. trailing lines): point at the first
        // unmatched tail.
        let exp_lines: Vec<&str> = expected.lines().collect();
        let act_lines: Vec<&str> = actual.lines().collect();
        let i = exp_lines.len().min(act_lines.len());
        let e = exp_lines.get(i).copied().unwrap_or("<EOF>").to_string();
        let a = act_lines.get(i).copied().unwrap_or("<EOF>").to_string();
        (i + 1, e, a)
    });

    panic!(
        "golden mismatch for `{name}`\n  fixture: {}\n  first diff at line {line}:\n    expected: {e_line}\n    actual:   {a_line}\n  hint: run `{UPDATE_ENV}=1 cargo test -p whitenoise-markdown --test golden` to update\n",
        json_path.display(),
    );
}

#[test]
fn kitchen_sink() {
    assert_golden("kitchen_sink");
}

#[test]
fn nostr_heavy() {
    assert_golden("nostr_heavy");
}

#[test]
fn tables_and_math() {
    assert_golden("tables_and_math");
}

#[test]
fn nested_containers() {
    assert_golden("nested_containers");
}

#[test]
fn bare_urls() {
    assert_golden("bare_urls");
}
