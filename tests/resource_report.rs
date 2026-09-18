//! Smoke test for the resource-measurement harness.
//!
//! Run: `timeout 200 cargo test --locked --test resource_report`
//!
//! Executes `scripts/resource-report.sh`, validates the machine-readable
//! report fields, and verifies the harness cleaned up after itself.

use std::process::Command;

#[test]
fn resource_report_fields_validate_and_cleanup_holds() {
    let output = Command::new("timeout")
        .args(["170", "scripts/resource-report.sh"])
        .output()
        .expect("failed to run resource-report.sh");
    assert!(
        output.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("report must be JSON on stdout");
    assert_eq!(report["schema"], "dg-resource-report/1");
    assert!(report["timestamp"].is_string());
    assert!(
        report["host"]["cpu_count"].as_u64().unwrap() >= 1,
        "host must report at least one CPU"
    );
    assert!(
        report["host"]["mem_total_kb"].as_u64().unwrap() > 0,
        "host must report total memory"
    );
    assert_eq!(report["workload"]["databases"], 3);
    assert_eq!(report["workload"]["rows_per_db"], 100);
    assert_eq!(report["workload"]["latency_probes"], 20);
    for key in ["idle", "loaded"] {
        assert!(
            report["memory_rss_kb"][key].as_u64().unwrap() > 0,
            "memory_rss_kb.{key} must be positive"
        );
    }
    assert!(
        report["latency_ms"]["min"].as_f64().unwrap()
            <= report["latency_ms"]["p50"].as_f64().unwrap()
            && report["latency_ms"]["p50"].as_f64().unwrap()
                <= report["latency_ms"]["max"].as_f64().unwrap(),
        "latency ordering min <= p50 <= max must hold"
    );
    assert!(report["disk_bytes"].as_u64().unwrap() > 0);
    assert!(
        !report["limitations"].as_array().unwrap().is_empty(),
        "limitations must be documented in the report"
    );
    assert!(
        report["budget_note"]
            .as_str()
            .unwrap()
            .contains("NOT a claim"),
        "report must disclaim production-budget compliance"
    );
    // Cleanup: the disposable workdir must be gone and no server left behind.
    let workdir = report["workdir"].as_str().unwrap().to_string();
    assert!(
        !std::path::Path::new(&workdir).exists(),
        "harness left its workdir behind: {workdir}"
    );
    let lingering = Command::new("pgrep")
        .args(["-f", "dg serve"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    assert!(!lingering, "harness left a server process running");
}
