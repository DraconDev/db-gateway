//! Dummy fleet: blog/CMS (posts / tags, many-read few-write).
//!
//! Run: `timeout 300 cargo test --locked --test fleet_blog`
//!
//! A small CMS driven through the real `dg` binary only: homepage
//! ordering, tag pages, slug lookup, LIKE search, tag rollup, publish
//! flow (draft goes live), keyed post insert with byte-identical replay,
//! and exact finale counts. Zero direct HTTP.

#[path = "common/mod.rs"]
mod common;

use common::Fleet;
use serde_json::json;

const DB: &str = "fleet_blog";

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
fn blog_renders_like_a_real_cms() {
    let fleet = Fleet::boot();
    assert_eq!(fleet.cli.ok("create", &["create", DB])["name"], DB);
    apply(&fleet, include_str!("../fleet/blog_schema.sql"));
    apply(&fleet, include_str!("../fleet/blog_seed.sql"));

    // Homepage: published posts newest first; the draft never appears.
    assert_eq!(
        fleet.cli.q(
            "homepage",
            DB,
            "SELECT slug FROM posts WHERE status = 'published' ORDER BY published_at DESC"
        ),
        json!([
            ["steady-report"],
            ["rust-notes"],
            ["gateway-bridge"],
            ["hello-world"],
            ["old-news"]
        ])
    );

    // Tag page: ops posts newest first.
    assert_eq!(
        fleet.cli.q(
            "tag page",
            DB,
            "SELECT p.slug FROM posts p JOIN post_tags pt ON pt.post_id = p.id JOIN tags t ON t.id = pt.tag_id WHERE t.name = 'ops' AND p.status = 'published' ORDER BY p.published_at DESC"
        ),
        json!([["steady-report"], ["gateway-bridge"]])
    );

    // Slug lookup serves the post body.
    assert_eq!(
        fleet.cli.q(
            "slug",
            DB,
            "SELECT title, body FROM posts WHERE slug = 'rust-notes'"
        ),
        json!([["Notes on Rust lifetimes", "Lifetimes without tears"]])
    );

    // Search across title and body.
    assert_eq!(
        fleet.cli.q(
            "search",
            DB,
            "SELECT slug FROM posts WHERE status = 'published' AND (title LIKE '%steady%' OR body LIKE '%steady%') ORDER BY slug"
        ),
        json!([["steady-report"]])
    );

    // Tags on one post, alphabetical.
    assert_eq!(
        fleet.cli.q(
            "post tags",
            DB,
            "SELECT t.name FROM tags t JOIN post_tags pt ON pt.tag_id = t.id WHERE pt.post_id = 6 ORDER BY t.name"
        ),
        json!([["rust"], ["tutorial"]])
    );

    // Tag rollup over published posts only.
    assert_eq!(
        fleet.cli.q(
            "tag rollup",
            DB,
            "SELECT t.name, count(*) FROM tags t JOIN post_tags pt ON pt.tag_id = t.id JOIN posts p ON p.id = pt.post_id AND p.status = 'published' GROUP BY t.name ORDER BY t.name"
        ),
        json!([["meta", 2], ["ops", 2], ["rust", 2], ["tutorial", 1]])
    );

    // Publish flow: the draft goes live on top of the homepage, tagged meta.
    fleet.cli.q(
        "publish",
        DB,
        "UPDATE posts SET status = 'published', published_at = '2026-04-01' WHERE slug = 'draft-notes'",
    );
    fleet.cli.q(
        "tag draft",
        DB,
        "INSERT INTO post_tags(post_id, tag_id) VALUES (4, 3)",
    );
    assert_eq!(
        fleet.cli.q(
            "homepage after publish",
            DB,
            "SELECT slug FROM posts WHERE status = 'published' ORDER BY published_at DESC LIMIT 2"
        ),
        json!([["draft-notes"], ["steady-report"]])
    );

    // Editor double-submit: same keyed insert twice is one row, byte-identical.
    let first = fleet.cli.qkey(
        "keyed post",
        DB,
        "INSERT INTO posts(slug, title, body, status, published_at) VALUES ('clippy-tips', 'Clippy tips', 'Zero warnings club', 'published', '2026-04-02')",
        "fleet-blog-1",
    );
    let replay = fleet.cli.qkey(
        "keyed post replay",
        DB,
        "INSERT INTO posts(slug, title, body, status, published_at) VALUES ('clippy-tips', 'Clippy tips', 'Zero warnings club', 'published', '2026-04-02')",
        "fleet-blog-1",
    );
    assert_eq!(first, replay, "keyed replay bytes drifted");

    // Finale: everything exact.
    assert_eq!(
        fleet.cli.q("posts", DB, "SELECT count(*) FROM posts"),
        json!([[7]])
    );
    assert_eq!(
        fleet.cli.q(
            "published",
            DB,
            "SELECT count(*) FROM posts WHERE status = 'published'"
        ),
        json!([[7]])
    );
    assert_eq!(
        fleet.cli.q("tags", DB, "SELECT count(*) FROM tags"),
        json!([[4]])
    );
    assert_eq!(
        fleet
            .cli
            .q("post_tags", DB, "SELECT count(*) FROM post_tags"),
        json!([[8]])
    );
}
