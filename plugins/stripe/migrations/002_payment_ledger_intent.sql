-- Version 2: the outbox intent a confirmed payment's ledger booking rides on.
--
-- Two changes, and one of them is why this is a NEW version rather than an edit
-- to version 1: `core.schema_migrations` records (schema, version, name), and the
-- runner skips a version it has already applied **without comparing its SQL**. An
-- amended version 1 would therefore be invisible on every deployed database while
-- looking perfectly correct on a fresh one. So the constraint is replaced here.
--
-- `ledger_status` gains exactly one value, and it means the only honest thing
-- this plugin knows between the charge and finance's answer:

--   intent_enqueued — the payment and its ledger intent committed together
--                     (core.outbox_enqueue ran as a VALUES expression in the same
--                     INSERT, so neither can exist without the other), and the
--                     core's relay will deliver it to POST /api/finance/transaction
--                     as svc.stripe.ledger. Neither booked nor unbooked.
--
-- Nothing existing is re-labelled: 'booked' still means finance confirmed the
-- entry, 'delegated_event' still means the payment.received hand-off with no
-- answer, and the pre-existing 'unbooked' rows stay the worklist's remaining job.

ALTER TABLE payments ADD COLUMN IF NOT EXISTS ledger_intent_id BIGINT;

-- One payment, one intent. The intent's idempotency key is the payment's own
-- identifier (stripe.payments.payment_id), so a second intent for one payment is
-- a bug rather than a retry -- and core.outbox_enqueue would return the first one
-- anyway. The index says so in the schema.
CREATE UNIQUE INDEX IF NOT EXISTS idx_stripe_payments_intent
    ON payments(ledger_intent_id) WHERE ledger_intent_id IS NOT NULL;

-- Replace the status check. PostgreSQL has no ALTER ... CHECK, so it is dropped
-- and re-added; the new set is the old five plus 'intent_enqueued'.
ALTER TABLE payments DROP CONSTRAINT IF EXISTS payments_status_valid;
ALTER TABLE payments ADD CONSTRAINT payments_status_valid
    CHECK (ledger_status IN ('unbooked', 'intent_enqueued', 'delegated_event', 'booked', 'refused', 'failed'));
