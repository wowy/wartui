//! The root `README.md` repeats the full keyboard list from this crate's README so an operator
//! sees it at a glance (AGENTS.md § Conventions). Nothing else ties the two copies together, so
//! this test fails when one changes without the other.

const MANUAL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"));
const ROOT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"));

/// Table rows from `## Keyboard commands` up to the next level-2 heading.
fn key_table_rows(doc: &str) -> Vec<&str> {
    doc.lines()
        .skip_while(|line| line.trim_end() != "## Keyboard commands")
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .map(str::trim_end)
        .filter(|line| line.starts_with('|'))
        .collect()
}

#[test]
fn readme_key_tables_match_manual_when_compared() {
    assert_eq!(
        key_table_rows(ROOT).join("\n"),
        key_table_rows(MANUAL).join("\n"),
        "README.md and crates/wartui/README.md keyboard tables differ; update both READMEs \
         together (AGENTS.md § Conventions)"
    );
}

#[test]
fn readme_key_tables_exist_when_extracted() {
    // Guards against a renamed or missing heading turning the comparison into two empty lists.
    // Both the main table and the settings table must be present.
    for (name, doc) in [("README.md", ROOT), ("crates/wartui/README.md", MANUAL)] {
        let rows = key_table_rows(doc).len();
        assert!(rows >= 10, "{name}: expected both keyboard tables, found {rows} table rows");
    }
}
