
CREATE TABLE IF NOT EXISTS checkout_sessions (
    id BIGSERIAL PRIMARY KEY,
    purpose TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'cad',
    member_id TEXT NOT NULL DEFAULT '',
    fund_code TEXT NOT NULL DEFAULT '',
    category TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    related_event_id TEXT NOT NULL DEFAULT '',
    dues_year INTEGER,
    status TEXT NOT NULL DEFAULT 'pending',
    stripe_session_id TEXT UNIQUE,
    checkout_url TEXT NOT NULL DEFAULT '',
    created_by TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT checkout_sessions_purpose_valid CHECK (purpose IN ('dues', 'donation', 'event_fee')),
    CONSTRAINT checkout_sessions_amount_positive CHECK (amount_cents > 0),
    CONSTRAINT checkout_sessions_status_valid CHECK (status IN ('pending', 'created', 'completed', 'expired', 'failed')),
    CONSTRAINT checkout_sessions_dues_year_valid CHECK (dues_year IS NULL OR dues_year BETWEEN 2000 AND 2200)
);
CREATE INDEX IF NOT EXISTS idx_stripe_sessions_status ON checkout_sessions(status, id DESC);
CREATE INDEX IF NOT EXISTS idx_stripe_sessions_member ON checkout_sessions(member_id);
CREATE TABLE IF NOT EXISTS webhook_events (
    id BIGSERIAL PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    livemode BOOLEAN NOT NULL DEFAULT false,
    api_version TEXT NOT NULL DEFAULT '',
    signature_timestamp BIGINT NOT NULL DEFAULT 0,
    payload_digest TEXT NOT NULL DEFAULT '',
    redeliveries INTEGER NOT NULL DEFAULT 0,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT webhook_events_redeliveries_valid CHECK (redeliveries >= 0)
);
CREATE INDEX IF NOT EXISTS idx_stripe_webhook_events_type ON webhook_events(event_type);
CREATE TABLE IF NOT EXISTS payments (
    id BIGSERIAL PRIMARY KEY,
    payment_id TEXT NOT NULL UNIQUE,
    event_id TEXT NOT NULL DEFAULT '',
    session_id TEXT NOT NULL DEFAULT '',
    purpose TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'cad',
    member_id TEXT NOT NULL DEFAULT '',
    fund_code TEXT NOT NULL DEFAULT '',
    category TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    dues_year INTEGER,
    livemode BOOLEAN NOT NULL DEFAULT false,
    ledger_status TEXT NOT NULL DEFAULT 'unbooked',
    ledger_mechanism TEXT NOT NULL DEFAULT '',
    ledger_transaction_id TEXT,
    ledger_error TEXT,
    confirmed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ledger_attempted_at TIMESTAMPTZ,
    CONSTRAINT payments_amount_positive CHECK (amount_cents > 0),
    CONSTRAINT payments_status_valid CHECK (ledger_status IN ('unbooked', 'delegated_event', 'booked', 'refused', 'failed')),
    CONSTRAINT payments_dues_year_valid CHECK (dues_year IS NULL OR dues_year BETWEEN 2000 AND 2200)
);
CREATE INDEX IF NOT EXISTS idx_stripe_payments_ledger ON payments(ledger_status, confirmed_at);
CREATE INDEX IF NOT EXISTS idx_stripe_payments_member ON payments(member_id);
