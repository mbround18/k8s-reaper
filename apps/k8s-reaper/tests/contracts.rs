//! The 001 contracts and the code name the same strings (FR-015, SC-007).
//!
//! Each check runs both ways: every string the code can produce is in the
//! contract, and every string in the contract is one the code produces.

use std::collections::BTreeSet;

use reaper_core::plan::{RestoreKind, SkipReason};
use reaper_core::ports::ActionKind;
use reaper_core::record::{PERSON_KEYS, RUN_HISTORY};

const ANNOTATIONS: &str =
    include_str!("../../../specs/001-crashloop-reaper/contracts/annotations.md");
const EVENTS: &str = include_str!("../../../specs/001-crashloop-reaper/contracts/events.md");
const METRICS: &str = include_str!("../../../specs/001-crashloop-reaper/contracts/metrics.md");

/// The first backticked cell of each body row of the table under the
/// heading named `heading`, up to the next heading.
fn table_keys(doc: &str, heading: &str) -> Vec<String> {
    let mut lines = doc.lines().skip_while(|line| {
        !(line.starts_with('#') && line.trim_start_matches('#').trim() == heading)
    });
    assert!(lines.next().is_some(), "no `{heading}` heading");
    let rows: Vec<&str> = lines
        .take_while(|line| !line.starts_with('#'))
        .filter(|line| line.trim_start().starts_with('|'))
        .collect();
    assert!(rows.len() > 2, "no table rows under `{heading}`");
    // Skip the header row and the separator row.
    rows[2..]
        .iter()
        .map(|row| {
            let cell = row
                .trim()
                .trim_start_matches('|')
                .split('|')
                .next()
                .unwrap_or("")
                .trim();
            backticked(cell).unwrap_or_else(|| {
                panic!("first cell under `{heading}` is not a backticked string: {row}")
            })
        })
        .collect()
}

fn backticked(cell: &str) -> Option<String> {
    let inner = cell.strip_prefix('`')?.strip_suffix('`')?;
    (!inner.is_empty() && !inner.contains('`')).then(|| inner.to_owned())
}

/// The values of the "`action` ∈ …;" list in the metrics table.
fn action_values(doc: &str) -> Vec<String> {
    let marker = "`action` ∈";
    let start = doc
        .find(marker)
        .expect("no \"`action` ∈\" list in metrics.md")
        + marker.len();
    let rest = &doc[start..];
    let list = &rest[..rest
        .find(';')
        .expect("the `action` list does not end at `;`")];
    list.split(',')
        .map(|item| {
            backticked(item.trim())
                .unwrap_or_else(|| panic!("`action` entry is not a backticked string: {item:?}"))
        })
        .collect()
}

/// The `reaper.boop.ninja/` keys of every annotations table, with the
/// "Set by" cell of their row.
fn annotation_rows(doc: &str) -> Vec<(String, String)> {
    doc.lines()
        .filter_map(|line| {
            let mut cells = line.trim().strip_prefix('|')?.split('|').map(str::trim);
            let key = backticked(cells.next()?)?;
            let set_by = cells.next()?.to_owned();
            key.starts_with("reaper.boop.ninja/")
                .then_some((key, set_by))
        })
        .collect()
}

/// Asserts the two sets are equal, naming what each side lacks.
fn assert_same(what: &str, contract: &[String], code: &[&str]) {
    let contract_set: BTreeSet<&str> = contract.iter().map(String::as_str).collect();
    assert_eq!(
        contract_set.len(),
        contract.len(),
        "{what}: the contract lists a string twice: {contract:?}"
    );
    let code_set: BTreeSet<&str> = code.iter().copied().collect();
    let missing: Vec<&str> = code_set.difference(&contract_set).copied().collect();
    let extra: Vec<&str> = contract_set.difference(&code_set).copied().collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{what}: missing from the contract (the code produces them): {missing:?}; \
         extra in the contract (the code never produces them): {extra:?}"
    );
}

#[test]
fn skip_reasons_match_the_events_contract() {
    let code: Vec<&str> = SkipReason::ALL.iter().map(|r| r.as_str()).collect();
    assert_same(
        "events.md Skip reasons",
        &table_keys(EVENTS, "Skip reasons"),
        &code,
    );
}

#[test]
fn restore_notes_match_the_events_contract() {
    let code: Vec<&str> = RestoreKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_same(
        "events.md Restore notes",
        &table_keys(EVENTS, "Restore notes"),
        &code,
    );
}

#[test]
fn actions_match_the_metrics_contract() {
    let code: Vec<&str> = ActionKind::ALL.iter().map(|a| a.as_str()).collect();
    assert_same("metrics.md `action` values", &action_values(METRICS), &code);
}

#[test]
fn person_keys_match_the_annotations_contract() {
    // `restore` is person-set, but the reaper removes it once handled.
    let person: Vec<String> = annotation_rows(ANNOTATIONS)
        .into_iter()
        .filter(|(key, set_by)| set_by == "person" && !key.ends_with("/restore"))
        .map(|(key, _)| key)
        .collect();
    assert_same("annotations.md person keys", &person, &PERSON_KEYS);
}

#[test]
fn run_history_is_a_reaper_key_in_the_annotations_contract() {
    let rows = annotation_rows(ANNOTATIONS);
    assert!(
        rows.iter()
            .any(|(key, set_by)| key == RUN_HISTORY && set_by == "reaper"),
        "annotations.md has no reaper-set `{RUN_HISTORY}` row"
    );
}

#[test]
fn the_parsers_reject_what_they_cannot_read() {
    let doc = "## T\n\n| A | B |\n| - | - |\n| `x` | y |\n| `y` | z |\n\n## Next\n| `n` | m |\n";
    assert_eq!(table_keys(doc, "T"), ["x", "y"]);
    let unquoted = "## T\n| A |\n| - |\n| x |\n";
    assert!(std::panic::catch_unwind(|| table_keys(unquoted, "T")).is_err());
    let list = "| `action` ∈ `a`, `b_c`; `result` ∈ `d` |";
    assert_eq!(action_values(list), ["a", "b_c"]);
    let rows = "| Key | Set by |\n| `reaper.boop.ninja/a` | person |\n| `other/b` | reaper |\n";
    assert_eq!(
        annotation_rows(rows),
        [("reaper.boop.ninja/a".to_owned(), "person".to_owned())]
    );
}
