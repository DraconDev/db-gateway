INSERT INTO flags(key, value, updated_at) VALUES
 ('checkout.v2', 'on', '2026-02-01T10:00:00'),
 ('dark.mode', 'off', '2026-02-01T10:00:00'),
 ('signup.captcha', 'on', '2026-02-01T10:00:00'),
 ('homepage.hero', 'variant-b', '2026-02-10T10:00:00'),
 ('api.rate_limit', '1000', '2026-02-01T10:00:00'),
 ('maintenance', 'off', '2026-02-01T10:00:00');
INSERT INTO flag_changes(key, old_value, new_value, changed_at) VALUES
 ('checkout.v2', 'off', 'on', '2026-02-01T10:00:00'),
 ('homepage.hero', 'variant-a', 'variant-b', '2026-02-10T10:00:00');
