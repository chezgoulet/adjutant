-- Version 4: the ledger is a record too, and the record guards cannot be
-- switched off by the role every plugin statement executes as (#152).
--
-- Why this is a new version: the runner records (schema, version, name) and
-- skips a version it has already applied without comparing its SQL, so an
-- amended version would be invisible on every deployed database while looking
-- correct on a fresh one. Version 3 is the receipts migration.
--
-- `finance.transactions` carried no trigger at all. The role every plugin
-- statement executes as could therefore UPDATE `amount_cents` or DELETE a row
-- carrying `external_ref` -- erasing an idempotency key and reopening the
-- double-book the ledger's uniqueness exists to prevent. The trigger below
-- refuses UPDATE and DELETE, exactly as `receipts_immutable` does for receipts.
--
-- A TRUNCATE fires no row-level trigger, so a statement-level guard refuses it
-- as well. Without it the whole ledger can be emptied in one statement and the
-- result looks exactly like a troop with no money.
--
-- A trigger alone is not enough while the writing role OWNS the table: an owner
-- can ALTER TABLE ... DISABLE TRIGGER. The core therefore moves ownership of
-- these record objects to the deployment role after this migration has run and
-- grants the plugin INSERT/SELECT only (server/src/schema.rs,
-- `protect_record_tables`). This migration installs the rules; ownership is
-- what keeps them armed.

CREATE OR REPLACE FUNCTION finance_transactions_are_records() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'finance.transactions is a record: ledger row % cannot be deleted. A correction is a new row, never an erased one.', OLD.id;
    END IF;
    RAISE EXCEPTION 'finance.transactions is a record: ledger row % cannot be modified. A correction is a new row, never an overwritten one.', OLD.id;
END;
$$;

DROP TRIGGER IF EXISTS transactions_immutable ON transactions;
CREATE TRIGGER transactions_immutable
    BEFORE UPDATE OR DELETE ON transactions
    FOR EACH ROW EXECUTE FUNCTION finance_transactions_are_records();

-- One statement-level guard for both record tables. TRUNCATE does not fire the
-- row triggers above, and a receipt table (or the ledger) that can be emptied
-- in one statement is not a record.
CREATE OR REPLACE FUNCTION finance_ledger_no_truncate() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is a record: TRUNCATE is refused', TG_TABLE_SCHEMA || '.' || TG_TABLE_NAME;
END;
$$;

DROP TRIGGER IF EXISTS transactions_no_truncate ON transactions;
CREATE TRIGGER transactions_no_truncate
    BEFORE TRUNCATE ON transactions
    FOR EACH STATEMENT EXECUTE FUNCTION finance_ledger_no_truncate();

DROP TRIGGER IF EXISTS receipts_no_truncate ON receipts;
CREATE TRIGGER receipts_no_truncate
    BEFORE TRUNCATE ON receipts
    FOR EACH STATEMENT EXECUTE FUNCTION finance_ledger_no_truncate();
