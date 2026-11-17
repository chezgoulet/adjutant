-- Version 4: automatic dues.
--
-- A member enrols once and Stripe charges them on an interval; each charge
-- arrives as `invoice.paid` and is booked against the same dues standing a
-- manual payment touches. This table is the shop's record of the arrangement —
-- what was agreed, at what amount, and whether it is still live.
--
-- `amount_cents` is the amount the member chose, which may be **less** than the
-- assessment: automatic does not mean "the full figure", it means "this amount,
-- on a schedule, without me opening a page each time".
CREATE TABLE IF NOT EXISTS subscriptions (
    id                     BIGSERIAL PRIMARY KEY,
    member_id              TEXT NOT NULL,
    fiscal_year            INTEGER NOT NULL,
    stripe_subscription_id TEXT UNIQUE,
    stripe_customer_id     TEXT NOT NULL DEFAULT '',
    status                 TEXT NOT NULL DEFAULT 'pending',
    amount_cents           BIGINT NOT NULL,
    currency               TEXT NOT NULL DEFAULT 'cad',
    interval               TEXT NOT NULL,
    fund_code              TEXT NOT NULL DEFAULT '',
    category               TEXT NOT NULL DEFAULT 'dues',
    -- Stripe's own clock: the next charge, and when the arrangement ended.
    current_period_end     TIMESTAMPTZ,
    cancelled_at           TIMESTAMPTZ,
    created_by             TEXT NOT NULL DEFAULT '',
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT subscriptions_status_valid CHECK (
        status IN ('pending', 'active', 'past_due', 'cancelled', 'incomplete')),
    CONSTRAINT subscriptions_interval_valid CHECK (interval IN ('year', 'month')),
    CONSTRAINT subscriptions_amount_positive CHECK (amount_cents > 0),
    CONSTRAINT subscriptions_year_valid CHECK (fiscal_year BETWEEN 2000 AND 2200)
);

CREATE INDEX IF NOT EXISTS idx_stripe_subscriptions_member
    ON subscriptions(member_id, fiscal_year);
CREATE INDEX IF NOT EXISTS idx_stripe_subscriptions_status
    ON subscriptions(status);
