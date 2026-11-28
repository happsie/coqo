-- Orders move from legacy_orders to orders.
CREATE TABLE orders (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  total NUMERIC(10, 2) NOT NULL,
  address TEXT NOT NULL,
  created_at TIMESTAMP NOT NULL DEFAULT now()
);

INSERT INTO orders (id, user_id, total, address, created_at)
  SELECT id, user_id, total, address, created_at FROM legacy_orders;

DROP TABLE legacy_orders;
