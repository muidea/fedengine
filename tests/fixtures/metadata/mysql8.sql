-- Dedicated disposable MySQL 8.0.44 database only.
CREATE DATABASE metadata;
USE metadata;
CREATE TABLE customers(tenant_id int NOT NULL,id int NOT NULL,name varchar(80) DEFAULT 'new',PRIMARY KEY(tenant_id,id));
CREATE TABLE orders(tenant_id int NOT NULL,id int NOT NULL AUTO_INCREMENT,customer_id int NOT NULL,amount decimal(18,2) DEFAULT 0,doubled decimal(18,2) GENERATED ALWAYS AS (amount*2) STORED,PRIMARY KEY(id,tenant_id),CONSTRAINT orders_customer_fk FOREIGN KEY(tenant_id,customer_id) REFERENCES customers(tenant_id,id) ON DELETE RESTRICT,CONSTRAINT amount_positive CHECK(amount>=0));
CREATE INDEX orders_amount_idx ON orders(amount DESC);
CREATE INDEX orders_expr_idx ON orders((ABS(amount)));
INSERT INTO customers VALUES(1,11,'JDBC customer');
INSERT INTO orders(tenant_id,id,customer_id,amount) VALUES(1,21,11,12.50);
