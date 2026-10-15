//! Release-bundle contract tests.
//!
//! The default micro2 test performs the real locked build and smoke test in
//! the immutable Ubuntu image. Focused fake-engine tests prove the default
//! path cannot silently fall back to a native build and that build/smoke
//! failures propagate.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};

const PINNED_IMAGE: &str = "docker.io/library/ubuntu@sha256:496754492fb28b4d3049432f2ca787449331e23fb14f0dd3fffea86bf5a93eb4";
static RELEASE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Release invocations share the repository target directory. Serialize them
/// so a real container build and the explicit host build cannot race and leave
/// a half-written Cargo build directory for the next invocation.
fn release_lock() -> MutexGuard<'static, ()> {
    RELEASE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

fn run_release(engine: Option<&Path>, target: &str) -> std::process::Output {
    let _guard = release_lock();
    let mut command = Command::new("timeout");
    command.args(["550", "scripts/release-bundle.sh"]);
    if let Some(engine) = engine {
        command.env("DG_CONTAINER_ENGINE", engine);
    }
    command.env("DG_RELEASE_TARGET", target);
    command.output().expect("run release-bundle.sh")
}

fn fake_engine(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-container-engine");
    fs::write(
        &path,
        format!("#!/usr/bin/env bash\nset -euo pipefail\n{body}\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn micro2_rejects_missing_container_engine_without_native_fallback() {
    let dir = tempdir();
    let missing = dir.path().join("missing-engine");
    let output = run_release(Some(&missing), "micro2");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requested container engine is unavailable"));
    assert!(
        output.stdout.is_empty(),
        "failed release must print no bundle path"
    );
}

#[test]
fn micro2_propagates_builder_inspection_failure() {
    let dir = tempdir();
    let engine = fake_engine(dir.path(), "echo 'builder drift' >&2; exit 42");
    let output = run_release(Some(&engine), "micro2");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("builder could not be inspected"));
    assert!(stderr.contains("builder drift"));
    assert!(output.stdout.is_empty());
}

#[test]
fn micro2_propagates_build_or_smoke_failure() {
    let dir = tempdir();
    let engine = fake_engine(
        dir.path(),
        r#"
if printf '%s\n' "$@" | grep -Fq 'cargo build --release --locked --quiet'; then
  echo 'smoke failed' >&2
  exit 43
fi
if printf '%s\n' "$@" | grep -Fq 'PRETTY_NAME'; then
  printf 'Ubuntu 24.04.5 LTS\nx86_64\nldd (Ubuntu GLIBC 2.39) 2.39\n'
  exit 0
fi
echo 'unexpected fake-engine invocation' >&2
exit 44
"#,
    );
    let output = run_release(Some(&engine), "micro2");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pinned-container release build or smoke test failed"));
    assert!(stderr.contains("smoke failed"));
    assert!(output.stdout.is_empty());
}

// `#[ignore]`d so `cargo test --workspace` does not run the full pinned-builder
// build on a contributor without podman/docker. The contract is exercised in
// CI by the `check` job of `.github/workflows/ci.yml`, which installs docker
// first and re-runs this single test with `--ignored`. The four sibling
// tests in this file still run on every PR: the three fake-engine modes
// (no engine, builder drift, build/smoke failure) prove the script's
// failure modes without needing a real container, and
// `explicit_host_bundle_never_claims_micro2_abi_verified` pins the
// `DG_RELEASE_TARGET=host` contract on every PR.
#[test]
#[ignore]
fn micro2_pinned_container_builds_bundle_and_records_builder() {
    let output = run_release(None, "micro2");
    assert!(
        output.status.success(),
        "bundle script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8(output.stdout).unwrap();
    let dir = PathBuf::from(out.lines().last().unwrap().trim());
    let bundle = |name: &str| dir.join(name);
    assert!(bundle("dg").is_file(), "bundle must contain dg");

    let verify = Command::new("sha256sum")
        .args(["-c", "SHA256SUMS"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        verify.status.success(),
        "checksum mismatch: {}",
        String::from_utf8_lossy(&verify.stdout)
    );

    let mode = fs::metadata(bundle("dg")).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "bundled dg must be executable");
    let help = Command::new(bundle("dg")).arg("--help").output().unwrap();
    assert!(help.status.success());
    let version = Command::new(bundle("dg"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("dg 0.1.0"));

    for name in ["README.md", "LICENSE", "BUILD-INFO.txt", "SHA256SUMS"] {
        assert!(bundle(name).is_file(), "bundle must contain {name}");
    }
    for name in ["docs/api.md", "docs/architecture.md"] {
        assert!(bundle(name).is_file(), "bundle must contain {name}");
    }
    let info = fs::read_to_string(bundle("BUILD-INFO.txt")).unwrap();
    for expected in [
        "target: micro2",
        "builder_image: ",
        PINNED_IMAGE,
        "builder_os: Ubuntu 24.04 amd64",
        "builder_glibc: glibc-2.39",
        "abi_verified: true",
        "container-built and smoke-tested",
    ] {
        assert!(
            info.contains(expected),
            "BUILD-INFO missing {expected:?}: {info}"
        );
    }
    assert!(!info.contains("current host architecture only"));

    fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists(), "bundle dir must be removed");
}

#[test]
fn explicit_host_bundle_never_claims_micro2_abi_verified() {
    let _guard = release_lock();
    let dir = tempdir();
    let out = dir.path().join("bundle");
    let mut command = Command::new("timeout");
    command.args(["550", "scripts/release-bundle.sh"]).arg(&out);
    command.env("DG_RELEASE_TARGET", "host");
    let output = command.output().expect("run host release bundle");
    assert!(
        output.status.success(),
        "host bundle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let info = fs::read_to_string(out.join("BUILD-INFO.txt")).unwrap();
    assert!(info.contains("target: host"));
    assert!(info.contains("builder_image: none"));
    assert!(info.contains("abi_verified: false"));
    assert!(info.contains("micro2 ABI is not verified"));
}

// The bundle's `dg` must be executable, and the deploy gate must be the
// independent second line that enforces it. `cp` applies the builder's
// umask to the new file, so a bundle staged under `umask 0111` (or a
// hand-staged artifact from a non-exec source) ships a 0644 `dg` that
// still passes `sha256sum -c` and the content-only portability checks --
// and then fails to exec under `User=dg` on the target. This is the same
// defect class as the RUNPATH check that closed the 2026-09-22 outage, so
// it is pinned here rather than left to the two chmod call sites staying
// in step by luck.
#[test]
fn staged_dg_must_be_executable_and_the_gate_enforces_it() {
    let _guard = release_lock();
    let dir = tempdir();
    let bundle = dir.path().join("bundle");
    fs::create_dir_all(&bundle).unwrap();

    // Build the bundle the way an operator does, then prove the shipped
    // artifact is executable by the target's own rule (mode 755, not just
    // "has some exec bit").
    let built = {
        let mut command = Command::new("timeout");
        command
            .args(["550", "scripts/release-bundle.sh"])
            .arg(&bundle);
        command.env("DG_RELEASE_TARGET", "host");
        let output = command.output().expect("run host release bundle");
        assert!(
            output.status.success(),
            "host bundle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        bundle.clone()
    };

    let mode = fs::metadata(built.join("dg")).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode & 0o111,
        0o111,
        "bundled dg is not executable (mode {mode:o}); release-bundle.sh must chmod it"
    );

    // Now prove the deploy gate discriminates: the same bundle with the
    // exec bit cleared must be refused by the gate's own check, not by
    // any later portability failure. Clearing the mode keeps every byte
    // identical, so SHA256SUMS no longer verifies -- re-stamp it so the
    // mode check is the ONLY thing that can fail.
    fs::set_permissions(built.join("dg"), fs::Permissions::from_mode(0o644)).unwrap();
    let sums = {
        let mut command = Command::new("bash");
        command.arg("-c").arg(format!(
            "cd {} && sha256sum dg > SHA256SUMS",
            built.display()
        ));
        command.output().expect("re-stamp SHA256SUMS")
    };
    assert!(sums.status.success(), "re-stamping SHA256SUMS failed");

    let output = {
        let mut command = Command::new("timeout");
        command.args(["60", "scripts/validate-deploy.sh"]);
        command.env("DG_RELEASE_BUNDLE_DIR", &built);
        command.output().expect("run validate-deploy.sh")
    };
    assert!(
        !output.status.success(),
        "the deploy gate accepted a non-executable bundle: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("has no executable dg binary"),
        "the gate failed for the wrong reason:\n{combined}"
    );

    // And the executable copy is accepted by the same mode check (it may
    // still fail later on this dev box's NixOS RUNPATH, which is correct
    // and is why only the mode verdict is asserted here).
    fs::set_permissions(built.join("dg"), fs::Permissions::from_mode(0o755)).unwrap();
    let sums = {
        let mut command = Command::new("bash");
        command.arg("-c").arg(format!(
            "cd {} && sha256sum dg > SHA256SUMS",
            built.display()
        ));
        command.output().expect("re-stamp SHA256SUMS")
    };
    assert!(sums.status.success(), "re-stamping SHA256SUMS failed");
    let output = {
        let mut command = Command::new("timeout");
        command.args(["60", "scripts/validate-deploy.sh"]);
        command.env("DG_RELEASE_BUNDLE_DIR", &built);
        command.output().expect("run validate-deploy.sh")
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("has no executable dg binary"),
        "the mode gate refused an executable bundle:\n{combined}"
    );
}
