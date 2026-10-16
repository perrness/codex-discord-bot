ALTER TABLE sessions DROP CONSTRAINT sessions_status_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_status_check CHECK (status IN
    ('provisioning','idle','running','waiting','stopping','recovering','unavailable','validating'));

CREATE TABLE issue_validations (
    id uuid PRIMARY KEY,
    run_id uuid NOT NULL REFERENCES issue_runs(id),
    turn_id text NOT NULL,
    policy jsonb,
    state text NOT NULL DEFAULT 'running' CHECK (state IN
        ('running','passed','failed','blocked','interrupted')),
    results jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(results)='array'),
    error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    UNIQUE(run_id, turn_id)
);
ALTER TABLE issue_runs ADD COLUMN repair_count integer NOT NULL DEFAULT 0 CHECK (repair_count BETWEEN 0 AND 5);
ALTER TABLE issue_runs ADD COLUMN validation_policy jsonb;
ALTER TABLE issue_runs ADD COLUMN validation_id uuid REFERENCES issue_validations(id);

-- Consumers must use orchestrator evidence, never a completed Codex turn alone.
CREATE VIEW validated_issue_runs AS
    SELECT r.id FROM issue_runs r JOIN issue_validations v ON v.id=r.validation_id AND v.run_id=r.id
    WHERE v.state='passed' AND v.finished_at IS NOT NULL AND v.policy IS NOT NULL
      AND jsonb_array_length(v.results)=3
      AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(v.results) result
          WHERE result->>'outcome' IS DISTINCT FROM 'passed'
             OR result->>'exit_code' IS DISTINCT FROM '0')
      AND v.results @> '[{"name":"build"},{"name":"test"},{"name":"lint"}]';
