INSERT INTO products(id, sku, name, price_cents, stock) VALUES
 (1, 'WIDGET-1', 'Widget', 1999, 50),
 (2, 'GADGET-2', 'Gadget', 4999, 20),
 (3, 'DOODAD-3', 'Doodad', 499, 200),
 (4, 'THING-4', 'Thingamajig', 12999, 5),
 (5, 'WHAT-5', 'Whatchamacallit', 799, 100);
INSERT INTO orders(id, customer, status, placed_at) VALUES
 (1, 'alice', 'paid', '2026-03-01'),
 (2, 'bob', 'open', '2026-03-02'),
 (3, 'alice', 'paid', '2026-03-03'),
 (4, 'cara', 'cancelled', '2026-03-04');
INSERT INTO line_items(id, order_id, product_id, qty, unit_price_cents) VALUES
 (1, 1, 1, 2, 1999),
 (2, 1, 3, 5, 499),
 (3, 2, 2, 1, 4999),
 (4, 3, 4, 1, 12999),
 (5, 3, 5, 3, 799),
 (6, 4, 1, 1, 1999),
 (7, 3, 1, 1, 1999);
