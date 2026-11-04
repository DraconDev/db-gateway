//! Dummy fleet: chat rooms (rooms / messages, double-send safety).
//!
//! Run: `timeout 300 cargo test --locked --test fleet_chat`
//!
//! A small chat app driven through the real `dg` binary only:
//! time-ordered room history, latest-per-room, since-token reads,
//! a keyed double-send stored once byte-identical, message edit,
//! message delete, FK-violation rejection, and exact finale counts.
//! Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

const DB: &str = "fleet_chat";

fn apply(fleet: &Fleet, sql_file: &str) {
    for stmt in sql_file.split(";\n") {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        fleet.cli.q("seed stmt", DB, stmt);
    }
}

#[test]
fn chat_reads_like_a_real_client() {
    let fleet = Fleet::boot();
    assert_eq!(fleet.cli.ok("create", &["create", DB])["name"], DB);
    apply(&fleet, include_str!("../fleet/chat_schema.sql"));
    apply(&fleet, include_str!("../fleet/chat_seed.sql"));

    // Room history in send order.
    assert_eq!(
        fleet.cli.q(
            "history",
            DB,
            "SELECT author, body FROM messages WHERE room_id = 2 ORDER BY sent_at, id"
        ),
        json!([
            ["dan", "Steady passed"],
            ["cara", "Nice, cutting over?"],
            ["dan", "Not yet, one more drill"],
            ["cara", "Drill green too"]
        ])
    );

    // Latest message per room.
    assert_eq!(
        fleet.cli.q(
            "latest",
            DB,
            "SELECT r.name, max(m.sent_at) FROM rooms r JOIN messages m ON m.room_id = r.id GROUP BY r.name ORDER BY r.name"
        ),
        json!([
            ["gateway-ops", "2026-03-01T09:10:00"],
            ["general", "2026-03-01T08:05:00"],
            ["random", "2026-03-01T12:30:00"]
        ])
    );

    // Message counts per room.
    assert_eq!(
        fleet.cli.q(
            "counts",
            DB,
            "SELECT r.name, count(m.id) FROM rooms r LEFT JOIN messages m ON m.room_id = r.id GROUP BY r.name ORDER BY r.name"
        ),
        json!([["gateway-ops", 4], ["general", 3], ["random", 5]])
    );

    // Since-token read: what arrived after 12:05 in random.
    assert_eq!(
        fleet.cli.q(
            "since",
            DB,
            "SELECT author, body FROM messages WHERE room_id = 3 AND sent_at > '2026-03-01T12:05:00' ORDER BY sent_at"
        ),
        json!([
            ["dan", "Back to work you two"],
            ["ben", "Fine..."],
            ["cara", "Lunch poll: pizza or sushi?"]
        ])
    );

    // Double-send: same keyed message twice is one row, byte-identical.
    let first = fleet.cli.qkey(
        "keyed message",
        DB,
        "INSERT INTO messages(room_id, author, body, sent_at) VALUES (1, 'ana', 'Standup in five', '2026-03-01T08:55:00')",
        "fleet-chat-1",
    );
    let replay = fleet.cli.qkey(
        "keyed message replay",
        DB,
        "INSERT INTO messages(room_id, author, body, sent_at) VALUES (1, 'ana', 'Standup in five', '2026-03-01T08:55:00')",
        "fleet-chat-1",
    );
    assert_eq!(first, replay, "keyed replay bytes drifted");
    assert_eq!(
        fleet.cli.q(
            "room1 count",
            DB,
            "SELECT count(*) FROM messages WHERE room_id = 1"
        ),
        json!([[4]])
    );

    // Edit a message in place.
    fleet.cli.q(
        "edit",
        DB,
        "UPDATE messages SET body = 'Deploy at noon, sharp' WHERE id = 3",
    );
    assert_eq!(
        fleet
            .cli
            .q("edited", DB, "SELECT body FROM messages WHERE id = 3"),
        json!([["Deploy at noon, sharp"]])
    );

    // Delete a message; room 3 drops to four.
    fleet
        .cli
        .q("delete", DB, "DELETE FROM messages WHERE id = 11");
    assert_eq!(
        fleet.cli.q(
            "room3 count",
            DB,
            "SELECT count(*) FROM messages WHERE room_id = 3"
        ),
        json!([[4]])
    );

    // Messages to phantom rooms are rejected, never stored.
    fleet.cli.err(
        "message bad room",
        &["query", DB, "INSERT INTO messages(room_id, author, body, sent_at) VALUES (999, 'x', 'ghost', '2026-03-01T00:00:00')"],
        "invalid_sql",
    );

    // Finale: everything exact (12 seeded + 1 sent - 1 deleted).
    assert_eq!(
        fleet.cli.q("rooms", DB, "SELECT count(*) FROM rooms"),
        json!([[3]])
    );
    assert_eq!(
        fleet.cli.q("messages", DB, "SELECT count(*) FROM messages"),
        json!([[12]])
    );
}
