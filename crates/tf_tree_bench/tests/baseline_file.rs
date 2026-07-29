//! The **committed** baseline, checked by the everyday suite.
//!
//! `just bench-check` is the real gate, and it needs a release build and a few
//! minutes. This file is the cheap half that runs in `cargo nextest run
//! --workspace`, and it exists because of one specific way the gate dies:
//! somebody bumps `report::SCHEMA`, every unit test still passes (they build
//! their own baselines from the live writer), and `just bench-check` — which
//! nobody runs on that PR — starts failing on the *next* branch for a reason
//! that looks unrelated.
//!
//! So this asserts the committed file is still a document this build could have
//! written: right schema, every §9.2 row present, and every metric carrying the
//! `drift` field the comparison needs. It does not compare any *value*; that is
//! `just bench-check`'s job and it needs a release build to be worth anything.

// Assertions are the point of a test binary.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::Value;
use tf_tree_bench::baseline::BASELINE_PATH;
use tf_tree_bench::report::{REQUIRED_ROWS, REQUIRED_WORSE, SCHEMA};

/// The committed baseline, parsed.
fn baseline() -> Value {
    // `CARGO_MANIFEST_DIR` is `crates/tf_tree_bench`; `BASELINE_PATH` is stated
    // from the workspace root, which is where every `just` recipe runs.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let path = root.join(BASELINE_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

/// The committed baseline is a document *this* build's writer could have
/// produced.
///
/// The schema assertion is the load-bearing one: a bump without a
/// `just bench-baseline-update` leaves a baseline that `compare` refuses
/// outright, and the refusal would first be seen by whoever next ran the gate,
/// on a branch that did not cause it.
///
/// Mutant (applied, confirmed fatal): bump `report::SCHEMA` to
/// `tf_tree.bench-report/3` without regenerating the baseline — this test fails
/// naming both spellings.
#[test]
fn the_committed_baseline_matches_this_builds_schema_and_row_set() {
    let b = baseline();
    assert_eq!(
        b["schema"].as_str(),
        Some(SCHEMA),
        "the committed baseline is a different schema from the one this build emits; \
         run `just bench-baseline-update`"
    );

    let rows: Vec<&str> = b["rows"]
        .as_array()
        .expect("`rows` array")
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    for id in REQUIRED_ROWS {
        assert!(
            rows.contains(id),
            "PHASE5 §9.2 row `{id}` is not in {rows:?}"
        );
    }
    let worse: Vec<&str> = b["where_we_are_worse"]
        .as_array()
        .expect("`where_we_are_worse` array")
        .iter()
        .filter_map(|w| w["id"].as_str())
        .collect();
    for id in REQUIRED_WORSE {
        assert!(worse.contains(id), "PHASE5 §9.3 entry `{id}` is missing");
    }
}

/// Every metric in the committed baseline carries a `drift` this build knows,
/// and at least one of them is directional.
///
/// A baseline whose metrics are all `informational` parses, compares cleanly and
/// gates nothing — the exact green-but-empty state `bench_report`'s
/// `checked == 0` guard also refuses at runtime. This is the same rule enforced
/// where it is cheap.
///
/// Mutant (applied, confirmed fatal): edit the committed
/// `differential_agreement.max_deviation.drift` to `"informational"` — the
/// second assertion fails.
#[test]
fn the_committed_baseline_still_gates_at_least_one_number() {
    let b = baseline();
    let mut directional = 0usize;
    for row in b["rows"].as_array().expect("`rows` array") {
        let id = row["id"].as_str().unwrap_or("?");
        for column in ["tf_tree", "tf2"] {
            for (key, m) in row[column].as_object().expect("metric object") {
                let drift = m["drift"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{id}.{column}.{key} records no `drift`"));
                assert!(
                    ["informational", "lower_is_better", "higher_is_better"].contains(&drift),
                    "{id}.{column}.{key} has an unknown drift `{drift}`"
                );
                if drift != "informational" {
                    directional += 1;
                    let tol = m["tolerance"]
                        .as_f64()
                        .unwrap_or_else(|| panic!("{id}.{column}.{key} records no `tolerance`"));
                    assert!(
                        tol.is_finite() && tol >= 0.0,
                        "{id}.{column}.{key} has tolerance {tol}"
                    );
                }
            }
        }
    }
    assert!(
        directional > 0,
        "the committed baseline gates no number at all, so `just bench-check` is green \
         without comparing anything"
    );
}
