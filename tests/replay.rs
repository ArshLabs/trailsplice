use serde_json::{Value, json};
use std::process::Command;
use trailsplice::{read_report, report_from_bytes};

const INCLUDED: &[u8] = include_bytes!("fixtures/inclusion.json");
const ABSENT: &[u8] = include_bytes!("fixtures/insufficient.json");
const GAP: &[u8] = include_bytes!("fixtures/gap_then_inclusion.json");

fn changed_report(change: impl FnOnce(&mut Value)) -> Result<String, Box<dyn std::error::Error>> {
    let mut value: Value = serde_json::from_slice(INCLUDED)?;
    change(&mut value);
    report_from_bytes(&serde_json::to_vec(&value)?)
}

#[test]
fn three_manual_cases_have_the_expected_conclusions_and_references() {
    let included = report_from_bytes(INCLUDED).unwrap();
    assert_eq!(
        included.lines().nth(3),
        Some("Saved receipts report inclusion according to this source.")
    );
    assert!(included.contains("#1: receipt reports block 42"));
    assert!(included.contains("execution successful"));
    let absent = report_from_bytes(ABSENT).unwrap();
    assert_eq!(
        absent.lines().nth(3),
        Some("No saved receipt establishes inclusion.")
    );
    assert!(absent.contains("#1: source returned no receipt."));
    assert!(absent.contains("#2: observer's wait timed out."));
    let gap = report_from_bytes(GAP).unwrap();
    assert!(gap.contains("#2: observation gap recorded"));
    assert!(gap.contains(
        "The next saved receipt is #3; inclusion timing relative to the gap is unknown."
    ));
    assert_eq!(gap, report_from_bytes(GAP).unwrap());
    for output in [&included, &absent, &gap] {
        assert!(output.contains("Evidence: constructed example"));
        assert!(output.contains(
            "This replay does not check the current chain or whether the block is finalized."
        ));
        assert_eq!(
            output
                .lines()
                .filter(|line| line.starts_with("Saved receipts")
                    || line.starts_with("No saved receipt"))
                .count(),
            1
        );
    }
}

#[test]
fn failed_execution_is_still_included_and_later_absence_preserves_history() {
    let output = changed_report(|value| {
        value["observations"][0]["receipt"]["status"] = json!("0x0");
        value["observations"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"receipt_absent"}));
    })
    .unwrap();
    assert_eq!(
        output.lines().nth(3),
        Some("Saved receipts report inclusion according to this source.")
    );
    assert!(output.contains("execution failed"));
    assert!(output.contains("#2: source returned no receipt"));
}

#[test]
fn conflicting_receipts_do_not_invent_a_reorg_or_choose_a_block() {
    for different_block in [true, false] {
        let output = changed_report(|value| {
            let mut second = value["observations"][0].clone();
            second["receipt"]["blockNumber"] = json!("0x2b");
            if different_block {
                second["receipt"]["blockHash"] =
                    json!("0x3333333333333333333333333333333333333333333333333333333333333333");
            }
            value["observations"].as_array_mut().unwrap().push(second);
        })
        .unwrap();
        assert_eq!(
            output.lines().nth(3),
            Some("Saved receipts disagree about the block or execution result.")
        );
        assert!(output.contains("#1: receipt reports block 42"));
        assert!(output.contains("#2: receipt reports block 43"));
        assert!(!output.contains("Saved receipts report inclusion"));
        assert!(!output.contains("No saved receipt establishes inclusion"));
        assert!(!output.contains("current inclusion"));
    }
}

#[test]
fn gap_references_follow_saved_order_and_identical_receipts_do_not_conflict() {
    let output = changed_report(|value| {
        let receipt = value["observations"][0].clone();
        value["observations"] = json!([
            {"kind":"observation_gap","reason":"first gap"},
            receipt,
            {"kind":"observation_gap","reason":"second gap"},
            receipt,
            {"kind":"observation_gap","reason":"last gap"}
        ]);
    })
    .unwrap();
    assert_eq!(
        output.lines().nth(3),
        Some("Saved receipts report inclusion according to this source.")
    );
    assert!(output.contains("The next saved receipt is #2;"));
    assert!(output.contains("The next saved receipt is #4;"));
    assert_eq!(output.matches("The next saved receipt").count(), 2);
    let last_gap = output
        .split("#5: observation gap recorded:")
        .nth(1)
        .unwrap();
    assert!(!last_gap.contains("The next saved receipt"));
    assert!(!output.contains("Saved receipts disagree"));
}

#[test]
fn invalid_evidence_is_rejected_before_a_report() {
    for change in [
        ("version", json!(2)),
        (
            "wrong hash",
            json!("0x3333333333333333333333333333333333333333333333333333333333333333"),
        ),
        ("quantity", json!("0x02a")),
        ("status", json!("0x2")),
        ("kind", json!("reorged")),
    ] {
        assert!(
            changed_report(|value| match change.0 {
                "version" => value["schema_version"] = change.1,
                "wrong hash" => value["observations"][0]["receipt"]["transactionHash"] = change.1,
                "quantity" => value["observations"][0]["receipt"]["blockNumber"] = change.1,
                "status" => value["observations"][0]["receipt"]["status"] = change.1,
                "kind" => value["observations"][0]["kind"] = change.1,
                _ => unreachable!(),
            })
            .is_err(),
            "{}",
            change.0
        );
    }
    assert!(report_from_bytes(b"{").is_err());
    assert!(report_from_bytes(&vec![b' '; 64 * 1024 + 1]).is_err());
    assert!(
        changed_report(
            |value| value["observations"] = json!(vec![json!({"kind":"receipt_absent"}); 129])
        )
        .is_err()
    );
    assert!(
        changed_report(
            |value| value["observations"] = json!([{"kind":"observation_gap","reason":"\n"}])
        )
        .is_err()
    );
}

#[test]
fn empty_evidence_and_boundary_inputs_have_explicit_results() {
    let output = changed_report(|value| value["observations"] = json!([])).unwrap();
    assert_eq!(
        output.lines().nth(3),
        Some("No saved receipt establishes inclusion.")
    );
    let mut boundary = ABSENT.to_vec();
    boundary.resize(64 * 1024, b' ');
    assert!(report_from_bytes(&boundary).is_ok());
    boundary.push(b' ');
    assert!(report_from_bytes(&boundary).is_err());
    assert!(
        changed_report(|value| {
            value["observations"] = json!(vec![json!({"kind":"receipt_absent"}); 128]);
        })
        .is_ok()
    );
    assert!(changed_report(|value| value["provenance"] = json!("captured_local")).is_err());
    assert!(changed_report(|value| value["source"] = json!("bad\nsource")).is_err());
    assert!(
        changed_report(|value| value["observations"][0]["receipt"]["blockHash"] = json!("0x22"))
            .is_err()
    );
    assert!(
        changed_report(|value| value["observations"][0]["receipt"]
            .as_object_mut()
            .unwrap()
            .remove("status")
            .map(|_| ())
            .unwrap())
        .is_err()
    );
}

#[test]
fn file_reader_rejects_excess_input_before_json_parsing() {
    let path = std::env::temp_dir().join(format!("trailsplice-limit-{}.json", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    use std::io::Write;
    file.write_all(&vec![b' '; 64 * 1024 + 1]).unwrap();
    drop(file);
    let result = read_report(&path);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(result.unwrap_err().to_string(), "evidence exceeds 64 KiB");
}

#[test]
fn command_line_replays_cases_and_reports_failures() {
    for (name, expected) in [
        ("inclusion", "Saved receipts report inclusion"),
        ("insufficient", "No saved receipt establishes inclusion"),
        ("gap_then_inclusion", "The next saved receipt is #3"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_trailsplice"))
            .args(["replay", &format!("tests/fixtures/{name}.json")])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
    for args in [vec!["replay", "does-not-exist.json"], vec![]] {
        let output = Command::new(env!("CARGO_BIN_EXE_trailsplice"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
