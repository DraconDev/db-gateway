INSERT INTO rooms(id, name, topic) VALUES
 (1, 'general', 'Watercooler'),
 (2, 'gateway-ops', 'Bridge status and drills'),
 (3, 'random', 'Anything goes');
INSERT INTO messages(id, room_id, author, body, sent_at) VALUES
 (1, 1, 'ana', 'Morning all', '2026-03-01T08:00:00'),
 (2, 1, 'ben', 'Morning', '2026-03-01T08:01:00'),
 (3, 1, 'ana', 'Deploy at noon?', '2026-03-01T08:05:00'),
 (4, 2, 'dan', 'Steady passed', '2026-03-01T09:00:00'),
 (5, 2, 'cara', 'Nice, cutting over?', '2026-03-01T09:02:00'),
 (6, 2, 'dan', 'Not yet, one more drill', '2026-03-01T09:03:00'),
 (7, 2, 'cara', 'Drill green too', '2026-03-01T09:10:00'),
 (8, 3, 'ben', 'Anyone tried the new cafe?', '2026-03-01T12:00:00'),
 (9, 3, 'ana', 'Yes, oat latte is great', '2026-03-01T12:05:00'),
 (10, 3, 'dan', 'Back to work you two', '2026-03-01T12:06:00'),
 (11, 3, 'ben', 'Fine...', '2026-03-01T12:07:00'),
 (12, 3, 'cara', 'Lunch poll: pizza or sushi?', '2026-03-01T12:30:00');
