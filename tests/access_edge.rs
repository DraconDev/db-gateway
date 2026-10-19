//! Negative tests for the non-loopback edge model (audit A9).
//!
//! The chosen edge (docs/access-edge.md) adds no new dg listener: an
//! encrypted overlay reaches a socket proxy, which reaches loopback.
//! That means the gateway's exposure model off-loopback is exactly
//! its credential model — so these tests pin it: a burst of bad keys
//! is uniformly rejected without disturbing legitimate traffic,
//! revoked keys die at once, and scoped keys stay in their lane. Key
//! guessing is not throttled by design (a 64-hex credential is not
//! guessable); what must hold is that rejecting guesses is cheap and
//! never locks out or degrades the real caller.

mod common;

use serde_json::Value;

fn get(server: &str, key: &str, path: &str) -> (u16, Value) {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!("{server}{path}"))
        .bearer_auth(key)
        .send()
        .expect("request sends");
    let status = resp.status().as_u16();
    let body: Value = resp.json().unwrap_or(Value::Null);
    (status, body)
}

#[test]
fn bad_key_burst_never_disturbs_legitimate_traffic() {
    let fleet = common::Fleet::boot();
    fleet.cli.ok("create db", &["create", "edge_neg_db"]);
    let scoped = fleet.cli.ok(
        "scoped key",
        &[
            "key",
            "create",
            "edge_reader",
            "--role",
            "database_read_only",
            "--database",
            "edge_neg_db",
        ],
    );
    let scoped_key = scoped["key"].as_str().unwrap().to_string();
    let bad_key = "ab".repeat(32);

    // 64 wrong-key attempts: every one a clean 401 with the stable
    // code — no 500s, no connection wedges, no information beyond
    // "invalid credential".
    for i in 0..64 {
        let (status, body) = get(&fleet.cli.server, &bad_key, "/v1/databases");
        assert_eq!(status, 401, "attempt {i}");
        assert_eq!(body["error"]["code"], "invalid_credential", "attempt {i}");
    }

    // Immediately after the burst, the real scoped caller is served
    // normally: rejecting guesses must not become a self-DoS.
    let (status, body) = get(&fleet.cli.server, &scoped_key, "/v1/databases");
    assert_eq!(status, 200);
    assert_eq!(body["databases"], serde_json::json!(["edge_neg_db"]));

    // Out-of-scope is a different wall than unauthenticated: the
    // scoped key asking for owner surface gets 403, not data.
    let (status, body) = get(&fleet.cli.server, &scoped_key, "/v1/keys");
    assert_eq!(status, 403);
    assert!(body["error"]["code"].is_string());

    // Revocation is immediate: the same key that just worked is dead.
    fleet.cli.ok("revoke", &["key", "revoke", "edge_reader"]);
    let (status, body) = get(&fleet.cli.server, &scoped_key, "/v1/databases");
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "invalid_credential");
}
