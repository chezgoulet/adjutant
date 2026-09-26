-- Receipts for money the troop receives (SPEC §7.5): a donation, a dues
-- payment, a purchase, a fee — any income the ledger holds can be receipted.
--
-- **A receipt is issued from the ledger record, never from a payment
-- provider.** Finance owns the money, so finance owns the receipt: a Stripe
-- Checkout session that funds a donation produces the finance transaction, and
-- the receipt follows the transaction. `transaction_id` is therefore NOT NULL
-- and references the ledger row, so a receipt for money that was never recorded
-- is unrepresentable rather than merely discouraged.
--
-- **A receipt is a record.** The triggers below refuse every UPDATE and every
-- DELETE on `receipts` — as `archive.records` and `conflicts.stage_log` do —
-- because a receipt that can be quietly edited is not a receipt. A correction is
-- a NEW row carrying `supersedes_id`, so what a giver was told is never
-- overwritten: both the superseded receipt and its correction remain, and the
-- pointer runs backwards from the correction (a receipt cannot be edited to name
-- its own successor, so `superseded_by` is derived on read).
--
-- **A receipt is addressed to the giver.** A member is named by their roster
-- identity (`member_id`); anyone else — a parent, a business — by the name they
-- gave, snapshotted into `payer_name` when the receipt is issued. There is
-- deliberately **no donor table**: what a receipt needs is recorded on the
-- receipt and nowhere else. This is not a donor CRM.
--
-- **`tax_statement` is the troop's own words, snapshotted.** It is empty unless
-- the troop has configured one, because a receipt claiming a deduction the troop
-- cannot substantiate is a liability for the troop, not a courtesy to the giver.
-- The software invents no acknowledgment; the statement is stored on the row, so
-- a later change of wording does not rewrite a receipt already given.
--
-- A new migration version (3), never an edit to an applied one: the runner skips
-- a version it has already applied without comparing its SQL, so amending an
-- applied version would be invisible on every deployed database while looking
-- correct on a fresh one. Version 2 is the dues funding migration, which was
-- merged ahead of this one, so this is 3. The statements are idempotent —
-- `IF NOT EXISTS`, `CREATE OR REPLACE FUNCTION`, `DROP TRIGGER IF EXISTS` — so
-- re-applying them is a no-op.

-- One numbering authority for the whole troop, and one that two treasurers
-- issuing at the same instant cannot collide over. A receipt number is unique
-- and increasing, not gapless: a gap is not a defect, a duplicate is.
CREATE SEQUENCE IF NOT EXISTS receipts_number_seq;

CREATE TABLE IF NOT EXISTS receipts (
    id BIGSERIAL PRIMARY KEY,
    -- `R-<fiscal year>-<sequence>`. Generated inside the issuing statement.
    number TEXT NOT NULL UNIQUE,
    fiscal_year INTEGER NOT NULL,
    -- The ledger entry this receipt is for. A receipt cannot exist for money
    -- that was never recorded, so this is NOT NULL and the FK is what says so.
    transaction_id BIGINT NOT NULL REFERENCES transactions(id),
    fund_id BIGINT NOT NULL REFERENCES funds(id),
    -- Snapshotted from the ledger entry at issue, so a later correction of the
    -- entry cannot silently restate what a giver was told.
    amount_cents BIGINT NOT NULL,
    issued_on DATE NOT NULL,
    -- The roster identity, when the giver is a member.
    member_id TEXT NOT NULL DEFAULT '',
    -- The name as they gave it, when the giver is not a member.
    payer_name TEXT NOT NULL DEFAULT '',
    purpose TEXT NOT NULL DEFAULT '',
    tax_statement TEXT NOT NULL DEFAULT '',
    -- The receipt this one corrects. NULL for a first receipt. `superseded_by`
    -- is derived (`SELECT c.id FROM receipts c WHERE c.supersedes_id = r.id`),
    -- never stored: a superseded receipt cannot be updated to point forward.
    supersedes_id BIGINT REFERENCES receipts(id),
    correction_reason TEXT NOT NULL DEFAULT '',
    issued_by TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT receipts_amount_positive CHECK (amount_cents > 0),
    CONSTRAINT receipts_addressee_present CHECK (member_id <> '' OR payer_name <> ''),
    CONSTRAINT receipts_no_self_supersede CHECK (supersedes_id IS NULL OR supersedes_id <> id),
    CONSTRAINT receipts_fiscal_year_valid CHECK (fiscal_year BETWEEN 2000 AND 2200)
);

CREATE INDEX IF NOT EXISTS idx_receipts_transaction ON receipts (transaction_id);
CREATE INDEX IF NOT EXISTS idx_receipts_member ON receipts (member_id, fiscal_year);
CREATE INDEX IF NOT EXISTS idx_receipts_year ON receipts (fiscal_year);
-- One correction per receipt: a second correction of the same receipt is a
-- correction of the correction, and this makes the ambiguity unrepresentable.
CREATE UNIQUE INDEX IF NOT EXISTS idx_receipts_one_correction
    ON receipts (supersedes_id) WHERE supersedes_id IS NOT NULL;

-- A receipt is a record: nothing legitimate needs UPDATE or DELETE, which is
-- why this can be unconditional rather than a list of protected columns a later
-- migration has to remember to extend.
CREATE OR REPLACE FUNCTION finance_receipts_are_records() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'finance.receipts is a record: receipt % cannot be deleted. Order-of-record corrections are issued with POST /api/finance/receipt/{id}/supersede, which supersedes it.', OLD.number;
    END IF;
    RAISE EXCEPTION 'finance.receipts is a record: receipt % cannot be modified. Order-of-record corrections are issued with POST /api/finance/receipt/{id}/supersede, which supersedes it.', OLD.number;
END;
$$;

DROP TRIGGER IF EXISTS receipts_immutable ON receipts;
CREATE TRIGGER receipts_immutable
    BEFORE UPDATE OR DELETE ON receipts
    FOR EACH ROW EXECUTE FUNCTION finance_receipts_are_records();

-- A correction corrects the same money. The superseded receipt must exist and
-- must be for the same ledger entry, so "correction" cannot quietly become a
-- second, unrelated receipt.
--
-- The lookup is **dynamic and schema-qualified through `TG_TABLE_SCHEMA`**, not
-- a static `SELECT … FROM receipts` with a `receipts` rowtype variable: a
-- plpgsql body resolves those against the *session's* `search_path`, and the
-- plugin's role carries `search_path = finance, public` (server/src/schema.rs)
-- while an operator's connection does not — so the static form would refuse the
-- rule for the plugin and fail with `type "receipts" does not exist` for anyone
-- else. A rule that only holds for one role is not a rule; this holds for every
-- writer.
CREATE OR REPLACE FUNCTION finance_receipt_correction_matches() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
DECLARE
    original_transaction_id BIGINT;
    original_number TEXT;
BEGIN
    IF NEW.supersedes_id IS NULL THEN
        RETURN NEW;
    END IF;
    EXECUTE format(
        'SELECT r.transaction_id, r.number FROM %I.receipts r WHERE r.id = $1',
        TG_TABLE_SCHEMA
    )
        INTO original_transaction_id, original_number
        USING NEW.supersedes_id;
    -- `transaction_id` is NOT NULL, so a NULL here is "no such row" stated
    -- positively — `FOUND` is not set by EXECUTE … INTO on every PostgreSQL
    -- this has run on, and a rule must not rest on the version's mood.
    IF original_transaction_id IS NULL THEN
        RAISE EXCEPTION 'receipt % supersedes receipt %, which does not exist', NEW.number, NEW.supersedes_id;
    END IF;
    IF original_transaction_id <> NEW.transaction_id THEN
        RAISE EXCEPTION 'receipt % supersedes receipt % for a different ledger entry: a correction corrects the same money', NEW.number, original_number;
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS receipts_correction_same_money ON receipts;
CREATE TRIGGER receipts_correction_same_money
    BEFORE INSERT ON receipts
    FOR EACH ROW EXECUTE FUNCTION finance_receipt_correction_matches();
