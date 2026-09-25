-- Fixture for `tests/macros.rs`: migration 2's SQL, distinct from migration 1's.
CREATE INDEX IF NOT EXISTS idx_greetings_message ON greetings (message);
