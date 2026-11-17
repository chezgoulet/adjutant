-- Version 3: the shop mirrors the dues standing finance announces, so a dues
-- checkout can be priced from what a member actually owes without the payer
-- holding a finance permission.
--
-- `finance.dues.assessed` and `finance.dues.payment` carry the figures; the
-- five-minute `dues_standing` sweep re-publishes them so a mirror that started
-- late still converges. Upserts are idempotent, which is why re-publishing the
-- whole troop costs nothing.
CREATE TABLE IF NOT EXISTS dues_mirror (
    member_id          TEXT NOT NULL,
    fiscal_year        INTEGER NOT NULL,
    assessed_cents     BIGINT NOT NULL DEFAULT 0,
    funded_cents       BIGINT NOT NULL DEFAULT 0,
    paid_cents         BIGINT NOT NULL DEFAULT 0,
    outstanding_cents  BIGINT NOT NULL DEFAULT 0,
    settled            BOOLEAN NOT NULL DEFAULT false,
    status             TEXT NOT NULL DEFAULT '',
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (member_id, fiscal_year),
    CONSTRAINT dues_mirror_year_valid CHECK (fiscal_year BETWEEN 2000 AND 2200),
    CONSTRAINT dues_mirror_figures_valid CHECK (
        assessed_cents >= 0 AND funded_cents >= 0 AND paid_cents >= 0
        AND outstanding_cents >= 0)
);

-- What the shop *assessed* the session against, next to what the payer chose to
-- pay. Both are kept because an under-payment is legitimate here (the owner's
-- ruling) and the shortfall has to be visible rather than inferred: a session
-- with `amount_cents < assessed_cents` is a member who paid less than they owed,
-- not a payment that went missing.
ALTER TABLE checkout_sessions ADD COLUMN IF NOT EXISTS assessed_cents BIGINT;
