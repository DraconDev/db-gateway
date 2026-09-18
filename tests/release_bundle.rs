//! Smoke test for the local release-bundle script.
//!
//! Run: `timeout 590 cargo test --locked --test release_bundle --release`
//! (debug profile works too but takes the same release build path).
//!
//! Builds the real bundle into a disposable directory, verifies every
//! checksum, runs the bundled `dg --help` / `dg --version`, then removes the
//! bundle directory.

use std::{os::unix::fs::PermissionsExt, process::Command};

#[test]
fn release_bundle_checksums_verify_and_binary_runs() {
    let output = Command::new("timeout")
        .args(["580", "scripts/release-bundle.sh"])
        .output()
        .expect("failed to run release-bundle.sh");
    assert!(
        output.status.success(),
        "bundle script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8(output.stdout).unwrap();
    let dir = std::path::PathBuf::from(out.lines().last().unwrap().trim());
    assert!(dir.join("dg").is_file(), "bundle must contain dg");

    // Every checksum verifies against the staged files.
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

    // The bundled binary is executable and self-describes.
    let mode = std::fs::metadata(dir.join("dg"))
        .unwrap()
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "bundled dg must be executable");
    let help = Command::new(dir.join("dg")).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("DB Gateway"),
        "bundled dg --help must describe the gateway"
    );
    let version = Command::new(dir.join("dg"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("dg 0.1.0"),
        "bundled dg --version must report the crate version"
    );

    // Required companions are staged.
    for name in ["README.md", "LICENSE", "BUILD-INFO.txt", "SHA256SUMS"] {
        assert!(dir.join(name).is_file(), "bundle must contain {name}");
    }
    for name in ["docs/api.md", "docs/architecture.md"] {
        assert!(dir.join(name).is_file(), "bundle must contain {name}");
    }
    assert!(
        std::fs::read_to_string(dir.join("BUILD-INFO.txt"))
            .unwrap()
            .contains("current host architecture only"),
        "BUILD-INFO must carry the no-cross-platform scope note"
    );

    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists(), "bundle dir must be removed");
}
