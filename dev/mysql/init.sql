USE dbtui;
SET SESSION cte_max_recursion_depth = 10000;
CREATE TABLE customers (
    id         INT AUTO_INCREMENT PRIMARY KEY,
    name       VARCHAR(100) NOT NULL,
    email      VARCHAR(255) UNIQUE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE orders (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    customer_id INT NOT NULL,
    total       DECIMAL(10,2) NOT NULL,
    status      VARCHAR(20) NOT NULL DEFAULT 'pending',
    FOREIGN KEY (customer_id) REFERENCES customers(id)
);
CREATE TABLE audit_log (message TEXT);  -- no primary key

INSERT INTO customers (name, email)
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 500)
SELECT CONCAT('Customer ', i), CONCAT('customer', i, '@example.com') FROM seq;
INSERT INTO orders (customer_id, total, status)
WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < 2000)
SELECT 1 + (i % 500), ROUND(RAND() * 500, 2), ELT(1 + i % 3, 'pending', 'paid', 'shipped') FROM seq;
INSERT INTO audit_log VALUES ('created'), ('updated');

-- Read-only user for permission testing
CREATE USER 'reader'@'%' IDENTIFIED BY 'reader';
GRANT SELECT ON dbtui.* TO 'reader'@'%';
