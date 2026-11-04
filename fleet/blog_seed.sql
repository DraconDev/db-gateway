INSERT INTO posts(id, slug, title, body, status, published_at) VALUES
 (1, 'hello-world', 'Hello world', 'First post on the new blog', 'published', '2026-01-05'),
 (2, 'gateway-bridge', 'Bridging to the gateway', 'How the gateway bridge works', 'published', '2026-02-10'),
 (3, 'steady-report', 'Two hours of steady', 'What the long steady taught us', 'published', '2026-03-15'),
 (4, 'draft-notes', 'Unfinished thoughts', 'Still thinking this one through', 'draft', NULL),
 (5, 'old-news', 'Last year in review', 'Looking back before moving on', 'published', '2025-12-01'),
 (6, 'rust-notes', 'Notes on Rust lifetimes', 'Lifetimes without tears', 'published', '2026-02-20');
INSERT INTO tags(id, name) VALUES (1, 'rust'), (2, 'ops'), (3, 'meta'), (4, 'tutorial');
INSERT INTO post_tags(post_id, tag_id) VALUES (1, 3), (2, 2), (2, 1), (3, 2), (5, 3), (6, 1), (6, 4);
