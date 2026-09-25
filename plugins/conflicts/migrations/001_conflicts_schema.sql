
CREATE TABLE IF NOT EXISTS cases (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    stage TEXT NOT NULL DEFAULT 'direct_conversation',
    status TEXT NOT NULL DEFAULT 'open',
    filed_by TEXT NOT NULL,
    party_ids TEXT[] NOT NULL DEFAULT '{}',
    facilitator_ids TEXT[] NOT NULL DEFAULT '{}',
    outcome TEXT NOT NULL DEFAULT '',
    agreement TEXT NOT NULL DEFAULT '',
    stage_since TIMESTAMPTZ NOT NULL DEFAULT now(),
    nudge_count INTEGER NOT NULL DEFAULT 0,
    nudged_at TIMESTAMPTZ,
    resolved_at TIMESTAMPTZ,
    resolved_by TEXT,
    withdrawn_at TIMESTAMPTZ,
    withdrawn_by TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT cases_stage_valid CHECK (stage IN (
        'direct_conversation', 'facilitation', 'arbitration', 'troop_council')),
    CONSTRAINT cases_status_valid CHECK (status IN ('open', 'resolved', 'withdrawn')),
    CONSTRAINT cases_parties_present CHECK (array_length(party_ids, 1) >= 1),
    CONSTRAINT cases_filer_is_a_party CHECK (filed_by = ANY(party_ids)),
    CONSTRAINT cases_resolution_recorded CHECK (status <> 'resolved' OR outcome <> ''),
    CONSTRAINT cases_withdrawal_recorded CHECK (status <> 'withdrawn' OR withdrawn_at IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS idx_cases_parties ON cases USING GIN (party_ids);
CREATE INDEX IF NOT EXISTS idx_cases_facilitators ON cases USING GIN (facilitator_ids);
CREATE INDEX IF NOT EXISTS idx_cases_stall ON cases (status, stage_since);

CREATE TABLE IF NOT EXISTS stage_log (
    id BIGSERIAL PRIMARY KEY,
    case_id BIGINT NOT NULL REFERENCES cases(id) ON DELETE RESTRICT,
    kind TEXT NOT NULL DEFAULT 'transition',
    from_stage TEXT NOT NULL DEFAULT '',
    to_stage TEXT NOT NULL DEFAULT '',
    actor TEXT NOT NULL,
    reason TEXT NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT stage_log_kind_valid CHECK (kind IN (
        'filed', 'transition', 'resolution', 'agreement', 'withdrawn',
        'facilitator_assigned', 'facilitator_released', 'party_added', 'nudge')),
    CONSTRAINT stage_log_reason_present CHECK (btrim(reason) <> '')
);
CREATE INDEX IF NOT EXISTS idx_stage_log_case ON stage_log (case_id, occurred_at, id);
