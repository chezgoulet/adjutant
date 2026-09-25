-- Fixture for `tests/macros.rs`: migration 1's SQL, in a file of its own.
CREATE TABLE IF NOT EXISTS greetings (
    id BIGSERIAL PRIMARY KEY,
    message TEXT NOT NULL
);
