-- Add to the dedicated PG16 baseline, then include these schemas in pg_baseline.
CREATE SCHEMA metadata;
CREATE TABLE metadata.orders(id integer PRIMARY KEY);
INSERT INTO metadata.orders VALUES(101);
CREATE SCHEMA alternate;
CREATE TABLE alternate.orders(id integer PRIMARY KEY);
INSERT INTO alternate.orders VALUES(202);
CREATE TABLE metadata."Order Line"(id integer PRIMARY KEY);
INSERT INTO metadata."Order Line" VALUES(303);
CREATE SCHEMA "MixedCase";
CREATE TABLE "MixedCase"."Order Line"(id integer PRIMARY KEY);
INSERT INTO "MixedCase"."Order Line" VALUES(404);
INSERT INTO sales.customers VALUES(1,11,'JDBC customer');
INSERT INTO sales.orders(tenant_id,id,customer_id,amount) VALUES(1,21,11,12.50);
