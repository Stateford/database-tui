CREATE TABLE customers (
    id         serial PRIMARY KEY,
    name       text NOT NULL,
    email      text UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE orders (
    id          serial PRIMARY KEY,
    customer_id int NOT NULL REFERENCES customers(id),
    total       numeric(10,2) NOT NULL,
    status      text NOT NULL DEFAULT 'pending'
);
CREATE TABLE audit_log (message text);  -- no primary key

INSERT INTO customers (name, email)
SELECT 'Customer ' || i, 'customer' || i || '@example.com' FROM generate_series(1, 500) i;
INSERT INTO orders (customer_id, total, status)
SELECT 1 + (i % 500), round((random() * 500)::numeric, 2), (ARRAY['pending','paid','shipped'])[1 + i % 3]
FROM generate_series(1, 2000) i;
INSERT INTO audit_log VALUES ('created'), ('updated');

CREATE SCHEMA reporting;
CREATE VIEW reporting.order_totals AS
SELECT c.name, count(o.id) AS orders, sum(o.total) AS total
FROM customers c LEFT JOIN orders o ON o.customer_id = c.id GROUP BY c.name;

-- Read-only user for permission testing
CREATE ROLE reader LOGIN PASSWORD 'reader';
GRANT USAGE ON SCHEMA public, reporting TO reader;
GRANT SELECT ON ALL TABLES IN SCHEMA public, reporting TO reader;
