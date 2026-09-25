
CREATE OR REPLACE FUNCTION conflicts_stage_log_append_only() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION
        'conflicts.stage_log is append-only (% refused): the history is the accountability', TG_OP;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS stage_log_append_only ON stage_log;
CREATE TRIGGER stage_log_append_only
    BEFORE UPDATE OR DELETE ON stage_log
    FOR EACH ROW EXECUTE FUNCTION conflicts_stage_log_append_only();
