-- Version 2: the outbox intent an order's scholarship draw rides on.
--
-- Two changes, and one of them is why this is a NEW version rather than an edit
-- to version 1: `core.schema_migrations` records (schema, version, name), and the
-- runner skips a version it has already applied **without comparing its SQL**. An
-- amended version 1 would therefore be invisible on every deployed database while
-- looking perfectly correct on a fresh one. So the constraint is replaced here.
--
-- `draw_status` gains exactly one value, and it means the only honest thing this
-- plugin knows between the order and finance's answer:

--   intent_enqueued — the order's completion (or comp) and its draw intent
--                     committed together (core.outbox_enqueue ran as an
--                     expression in the same UPDATE, so neither can exist
--                     without the other), and the core's relay will deliver it
--                     to POST /api/finance/transfer as svc.store.draw. Neither
--                     booked nor unbooked.
--
-- Nothing existing is re-labelled: 'booked' still means finance confirmed the
-- transfer, 'unbooked' still means a funded order no caller has booked (the
-- pre-existing rows, and a reduction a treasurer books by hand), and
-- 'attempting'/'refused'/'failed' keep their meanings on the caller-driven path.

ALTER TABLE orders ADD COLUMN IF NOT EXISTS draw_intent_id BIGINT;

-- One order, one draw intent. The intent's idempotency key is the order's own
-- identifier (`store-order-<id>-draw`), so a second intent for one order is a bug
-- rather than a retry -- and core.outbox_enqueue would return the first one
-- anyway. The index says so in the schema.
CREATE UNIQUE INDEX IF NOT EXISTS idx_store_orders_draw_intent
    ON orders(draw_intent_id) WHERE draw_intent_id IS NOT NULL;

-- Replace the status check. PostgreSQL has no ALTER ... CHECK, so it is dropped
-- and re-added; the new set is the old six plus 'intent_enqueued'.
ALTER TABLE orders DROP CONSTRAINT IF EXISTS store_orders_draw_status_valid;
ALTER TABLE orders ADD CONSTRAINT store_orders_draw_status_valid
    CHECK (draw_status IN ('none', 'unbooked', 'intent_enqueued', 'attempting', 'booked', 'refused', 'failed'));
