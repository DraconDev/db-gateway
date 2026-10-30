//! `dg key revoke --database`: cut every credential scoped to one database.
//!
//! Run: `timeout 300 cargo test --locked --test cli_key_bulk_revoke`
//!
//! The contract these tests pin: a leaked key is cut in ONE command, and
//! the command that cuts it never takes away the key the operator is
//! holding. The owner credential reaches every database by construction,
//! so a naive "revoke everything scoped here" sweep would lock the owner
//! out of their own gateway — that is the failure this pins hardest.
//!
//! The second contract is that the bulk form is a different verb from the
//! single form: `dg key revoke NAME` still revokes exactly one key, and
//! the two cannot be confused for each other.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::Value;

/// Two databases, each with its own credentials, so a sweep can be shown
/// to hit one scope and leave the other alone.
fn seed(fleet: &Fleet) {
    fleet.cli.ok("create shop", &["create", "shop"]);
    fleet.cli.ok("create lab", &["create", "lab"]);
    for (name, db) in [
        ("shop_app", "shop"),
        ("shop_report", "shop"),
        ("lab_app", "lab"),
    ] {
        fleet.cli.ok(
            name,
            &[
                "key",
                "create",
                name,
                "--role",
                "database_read_write",
                "--database",
                db,
            ],
        );
    }
}

/// The names of the credentials the gateway lists.
fn key_names(fleet: &Fleet) -> Vec<String> {
    let listed = fleet.cli.ok("keys", &["key", "list"]);
    listed["keys"]
        .as_array()
        .expect("keys list")
        .iter()
        .filter_map(|key| key["name"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn a_database_sweep_revokes_every_key_scoped_to_it() {
    let fleet = Fleet::boot();
    seed(&fleet);
    assert!(key_names(&fleet).contains(&"shop_app".to_string()));

    let revoked = fleet.cli.ok(
        "bulk",
        &["key", "revoke", "--database", "shop", "--confirm", "shop"],
    );

    // The names are reported, not just a count: after a leak the
    // operator needs to know WHICH credentials went.
    let names: Vec<&str> = revoked["revoked"]
        .as_array()
        .expect("revoked list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(names.contains(&"shop_app"), "{names:?}");
    assert!(names.contains(&"shop_report"), "{names:?}");

    let after = key_names(&fleet);
    assert!(!after.contains(&"shop_app".to_string()), "{after:?}");
    assert!(!after.contains(&"shop_report".to_string()), "{after:?}");
}

#[test]
fn a_database_sweep_never_revokes_the_owner_key() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // The owner key reaches every database, so it is in scope for the
    // question "who can reach shop". It must not be in scope for the
    // answer "revoke them": taking it away would lock the operator out
    // of the gateway they were trying to protect.
    let revoked = fleet.cli.ok(
        "bulk",
        &["key", "revoke", "--database", "shop", "--confirm", "shop"],
    );
    let names: Vec<&str> = revoked["revoked"]
        .as_array()
        .expect("revoked list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !names.contains(&"owner"),
        "the owner key was swept up: {names:?}"
    );
    assert_eq!(
        revoked["owner_key_excluded"].as_bool(),
        Some(true),
        "JSON must make the owner-key safety property explicit: {revoked}"
    );

    // And the owner key still works: a command that needs it succeeds.
    let listed = fleet.cli.ok("still owner", &["key", "list"]);
    assert!(listed["keys"].is_array(), "owner credential is gone");
}

#[test]
fn a_database_sweep_leaves_other_databases_alone() {
    let fleet = Fleet::boot();
    seed(&fleet);
    fleet.cli.ok(
        "bulk",
        &["key", "revoke", "--database", "shop", "--confirm", "shop"],
    );
    let after = key_names(&fleet);
    // `lab_app` reaches a different database and had nothing to do with
    // the leak: revoking it would be collateral damage.
    assert!(
        after.contains(&"lab_app".to_string()),
        "the sweep reached a database it was not asked about: {after:?}"
    );
}

#[test]
fn a_database_with_no_scoped_keys_revokes_nothing_and_says_so() {
    let fleet = Fleet::boot();
    fleet.cli.ok("create quiet", &["create", "quiet"]);
    let revoked = fleet
        .cli
        .ok(
            "bulk",
            &["key", "revoke", "--database", "quiet", "--confirm", "quiet"],
        );
    let names = revoked["revoked"].as_array().expect("revoked list");
    assert!(
        names.is_empty(),
        "a database with no application keys revoked something: {names:?}"
    );
}

#[test]
fn bulk_revoke_requires_exact_confirmation_and_explains_owner_exclusion() {
    let fleet = Fleet::boot();
    seed(&fleet);

    let (ok, stdout, stderr) = fleet.cli.run(&["key", "revoke", "--database", "shop"]);
    assert!(!ok, "bulk revoke ran without confirmation: {stdout}{stderr}");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("confirmation_required"),
        "missing confirmation reason: {combined}"
    );
    assert!(
        combined.contains("owner key is never revoked"),
        "confirmation text must state the owner-key exclusion: {combined}"
    );

    // A wrong confirmation is not consent to revoke a scope.
    let (ok, stdout, stderr) = fleet
        .cli
        .run(&["key", "revoke", "--database", "shop", "--confirm", "lab"]);
    assert!(!ok, "wrong confirmation accepted: {stdout}{stderr}");
}

#[test]
fn the_single_key_form_still_revokes_exactly_one() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // The bulk form is a different verb. `dg key revoke NAME` must not
    // widen into a scope sweep, or a typo would cut more than intended.
    fleet.cli.ok("one", &["key", "revoke", "shop_app"]);
    let after = key_names(&fleet);
    assert!(!after.contains(&"shop_app".to_string()), "{after:?}");
    assert!(
        after.contains(&"shop_report".to_string()),
        "revoking one key took its neighbour: {after:?}"
    );
}

#[test]
fn the_two_forms_cannot_be_combined() {
    let fleet = Fleet::boot();
    seed(&fleet);
    // A caller that means one key and passes both must be refused
    // rather than guessed at: the two forms do very different amounts
    // of damage.
    let (ok, stdout, stderr) = fleet
        .cli
        .run(&["key", "revoke", "shop_app", "--database", "shop"]);
    assert!(
        !ok,
        "both forms accepted at once: stdout={stdout} stderr={stderr}"
    );
}
