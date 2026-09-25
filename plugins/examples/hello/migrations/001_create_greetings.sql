CREATE TABLE IF NOT EXISTS greetings (
    id BIGSERIAL PRIMARY KEY,
    message TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS events_received (
    id BIGSERIAL PRIMARY KEY,
    event_type TEXT NOT NULL,
    payload TEXT,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
