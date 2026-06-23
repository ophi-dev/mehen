-- Migration script: structurally simple, operationally risky.
BEGIN;

DROP TABLE IF EXISTS legacy_orders;

TRUNCATE TABLE staging_events;

ALTER TABLE customers ADD COLUMN loyalty_tier VARCHAR(20);

UPDATE customers SET loyalty_tier = 'bronze';

DELETE FROM sessions;

GRANT SELECT ON customers TO reporting_role;

COMMIT;
