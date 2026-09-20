//! Contract test for the local filesystem-backed Profile C rehearsal.
//!
//! Run: `cargo test --locked --test recovery_rehearsal`
//!
//! The binary deliberately does not contact Litestream, OVH, Turso, or
//! micro2; it exercises the gateway's export/import trust boundary locally.

use serde_json::Value;
use std::process::Command;

#[test]
fn local_rehearsal_reports_all_faults_and_restore_evidence() {
    let output = Command::new(env!("CARGO_BIN_EXE_recovery_rehearsal"))
        .output()
        .expect("failed to run recovery rehearsal");
    assert!(
        output.status.success(),
        "rehearsal failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("report must be JSON");
    assert_eq!(report["schema"], "dg-recovery-rehearsal/1");
    assert_eq!(report["scope"], "local filesystem object mock only");
    assert_eq!(report["policy"]["profile"], "C");
    assert_eq!(report["policy"]["lag_objective_seconds"], 300);
    assert_eq!(report["policy"]["rto_objective_seconds"], 900);
    assert_eq!(report["registry_artifact"]["verified"], true);

    let scenarios = report["scenarios"].as_array().expect("scenario array");
    assert_eq!(scenarios.len(), 4);
    for scenario in scenarios {
        assert_eq!(scenario["passed"], true, "scenario failed: {scenario}");
    }
    assert_eq!(
        scenarios[0]["failed_status"]["status"]["state"],
        "failed"
    );
    assert_eq!(
        scenarios[0]["stale_status_after_failure_clear"]["status"]["state"],
        "stale"
    );
    assert_eq!(
        scenarios[1]["restore_rejected_without_publication"]["published"],
        false
    );
    assert_eq!(scenarios[2]["writes_after_outage"], true);
    assert!(
        scenarios[3]["wal_bytes_while_reader_held"]
            .as_u64()
            .expect("WAL measurement")
            > 0
    );
    assert_eq!(report["fallback"]["passed"], true);
    assert_eq!(report["fallback"]["fresh_status"]["status"]["state"], "fresh");

    let baseline_rto = report["baseline"]["restore"]["rto_ms"]
        .as_u64()
        .expect("local RTO measurement");
    assert!(baseline_rto < 900_000, "local RTO exceeded 15 minutes");
    assert_eq!(report["baseline"]["restore"]["exact_rows"], true);
    assert_eq!(report["baseline"]["restore"]["independent"], true);
}
