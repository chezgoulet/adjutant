-- Version 2: a dues row names what `scholarship` funded, and the draw's state.
--
-- This is a NEW version rather than an edit to version 1, and the reason is the
-- runner's own rule: `core.schema_migrations` records (schema, version, name) and
-- a version that has already been applied is **skipped without its SQL being
-- compared** (server/src/db.rs, server/src/plugin_runtime.rs). An amended version
-- 1 would therefore change nothing on every deployed database while looking
-- perfectly correct on a fresh one.
--
-- `funded_cents` is the comp's third figure, the one a dues row was missing:
-- `base_cents` is the troop's membership cost, `assessed_cents` is the tier's
-- share, and `funded_cents` is what `scholarship` covers. The member's own share
-- is `assessed_cents - funded_cents`. A waiver is the case where that share is 0.

ALTER TABLE dues ADD COLUMN IF NOT EXISTS funded_cents BIGINT NOT NULL DEFAULT 0;

-- The draw's own state, in the vocabulary the store already uses for a
-- scholarship draw (plugins/store/src/lib.rs) -- one vocabulary for a draw,
-- whichever plugin booked it:
--
--   none       -- nothing to fund: the assessment is zero, or the tier funds
--                 nothing at all (a patron assessing above the base cost).
--   unbooked   -- funded, and no caller holding `finance:write` has booked the
--                 transfer yet -- the worklist a treasurer closes by hand.
--   attempting -- a `finance:write` caller is booking it in this flow.
--   booked     -- the transfer landed; `draw_ref` carries its transfer group.
--   refused    -- finance's own transfer route refused it (the configured dues
--                 fund IS `scholarship`, so the subsidy is already where it
--                 would go). Nothing was booked, and the waiver still stands.
--   failed     -- the transfer call errored. Nothing was booked.
--
-- Narrowed again in the amount: a draw is never zero, so a zero-valued subsidy
-- books no transaction and records `none` instead.
ALTER TABLE dues ADD COLUMN IF NOT EXISTS draw_status TEXT NOT NULL DEFAULT 'none';
ALTER TABLE dues DROP CONSTRAINT IF EXISTS dues_draw_status_valid;
ALTER TABLE dues ADD CONSTRAINT dues_draw_status_valid
    CHECK (draw_status IN ('none', 'unbooked', 'attempting', 'booked', 'refused', 'failed'));

-- The transfer group the draw landed under, once it has one: finance's own
-- identifier for the money move, carried on the row that asked for it.
ALTER TABLE dues ADD COLUMN IF NOT EXISTS draw_ref UUID;

-- Replace the waived-with-zero rule. PostgreSQL has no ALTER ... CHECK, so it is
-- dropped and re-added -- and the invariant is KEPT, re-expressed in terms of the
-- member's own share rather than dropped with the column that forced a zero:
--
--   dues_funded_valid              funded_cents >= 0
--   dues_funded_within_assessment  funded_cents <= assessed_cents
--   dues_waived_is_funded          status <> 'waived' OR funded_cents = assessed_cents
--
-- "Waived but owing" stays unrepresentable: what a waived member owes is
-- `assessed_cents - funded_cents`, and `dues_waived_is_funded` makes that 0, so
-- it can never go negative. What is gained is that a waiver now names what was
-- funded.
--
-- Every waiver in a deployed database is `assessed_cents = 0` -- that is exactly
-- what the old constraint enforced -- so `funded_cents = 0` (the column's default)
-- satisfies all three new constraints and the swap applies cleanly to every
-- existing row.
--
-- The tier -> basis-point scale is deliberately NOT re-encoded here: the scale is
-- Rust's (`TIERS`, `tier_assessment`), and a SQL copy of it would be the second
-- scale this whole rule exists to avoid. The database keeps the invariant without
-- restating the arithmetic.
ALTER TABLE dues DROP CONSTRAINT IF EXISTS dues_waived_is_zero;
ALTER TABLE dues DROP CONSTRAINT IF EXISTS dues_funded_valid;
ALTER TABLE dues ADD CONSTRAINT dues_funded_valid CHECK (funded_cents >= 0);
ALTER TABLE dues DROP CONSTRAINT IF EXISTS dues_funded_within_assessment;
ALTER TABLE dues ADD CONSTRAINT dues_funded_within_assessment
    CHECK (funded_cents <= assessed_cents);
ALTER TABLE dues DROP CONSTRAINT IF EXISTS dues_waived_is_funded;
ALTER TABLE dues ADD CONSTRAINT dues_waived_is_funded
    CHECK (status <> 'waived' OR funded_cents = assessed_cents);

-- A draw is one balanced transfer per waiver, so the reference it carries is
-- unique. Nothing here recomputes a historic row: the tier's share is a Rust
-- computation (`tier_assessment`) and SQL must not guess it, so the rows already
-- waived at zero are repaired by an explicit, guarded, idempotent route in the
-- plugin (`POST /api/finance/dues/repair-waivers`, `finance:manage`) -- on demand,
-- never at startup, because rewriting a past year's report is the owner's call.
-- Until it is run, a row reads "waived, nothing to fund" rather than a figure
-- nobody computed, and the worklist names the rows it has not touched. A row whose
-- `base_cents` is 0 has nothing to fund and is left alone by the repair too.
CREATE UNIQUE INDEX IF NOT EXISTS idx_dues_draw_ref
    ON dues(draw_ref) WHERE draw_ref IS NOT NULL;

-- The outstanding-draw worklist reads this: the funded rows whose draw has not
-- been booked.
CREATE INDEX IF NOT EXISTS idx_dues_draw_status
    ON dues(draw_status) WHERE draw_status <> 'none';
