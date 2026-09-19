INSERT INTO projects(id, name, archived) VALUES (1, 'Website relaunch', 0), (2, 'Gateway bridge', 0), (3, 'Old intranet', 1);
INSERT INTO tasks(id, project_id, title, status, priority, assignee) VALUES
 (1, 1, 'Draft homepage copy', 'done', 2, 'ana'),
 (2, 1, 'Pick typeface', 'done', 4, 'ben'),
 (3, 1, 'Build photo gallery', 'in_progress', 1, 'ana'),
 (4, 1, 'Write launch post', 'open', 3, NULL),
 (5, 2, 'Define JSON API', 'done', 1, 'cara'),
 (6, 2, 'Keyed replay drill', 'in_progress', 1, 'dan'),
 (7, 2, 'Soak the bridge', 'open', 2, 'dan'),
 (8, 2, 'Write cutover notes', 'open', 3, 'cara'),
 (9, 3, 'Archive meeting notes', 'done', 5, NULL),
 (10, 3, 'Export wiki pages', 'open', 4, NULL),
 (11, 1, 'Compress hero images', 'open', 2, 'ben'),
 (12, 2, 'Renew test cert', 'open', 5, NULL);
INSERT INTO comments(id, task_id, author, body) VALUES
 (1, 3, 'ana', 'Gallery grid is up on staging'),
 (2, 3, 'ben', 'Reviewed and left two nits'),
 (3, 6, 'dan', 'First replay drill passed'),
 (4, 6, 'cara', 'Second drill covers restarts'),
 (5, 6, 'dan', 'Restart drill passed too'),
 (7, 4, 'ana', 'Outline is ready for review'),
 (8, 11, 'ben', 'Saved 400KB with no visible loss'),
 (9, 7, 'dan', 'Soak plan drafted');
