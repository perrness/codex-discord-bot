-- Existing attempts keep their pinned managed pipeline. New attempts use the
-- repository's instructions and authorized Discord follow-ups by default.
ALTER TABLE issue_runs ADD COLUMN workflow text NOT NULL DEFAULT 'managed'
    CHECK (workflow IN ('repository','managed'));
ALTER TABLE issue_runs ALTER COLUMN workflow SET DEFAULT 'repository';
UPDATE issue_runs SET workflow='repository' WHERE state='queued';
ALTER TABLE issue_runs DROP CONSTRAINT issue_runs_state_check;
ALTER TABLE issue_runs ADD CONSTRAINT issue_runs_state_check CHECK (state IN
    ('queued','preparing','implementing','awaiting_input','completed','validating',
     'integrating','releasing','released','blocked','failed','cancelled'));

-- User confirmation is distinct from independent managed-release evidence.
CREATE TABLE issue_workflow_completions (
    run_id uuid PRIMARY KEY REFERENCES issue_runs(id),
    confirmed_by text NOT NULL,
    confirmed_at timestamptz NOT NULL DEFAULT now()
);

CREATE OR REPLACE VIEW issue_worker_slots AS SELECT r.* FROM issue_runs r
    WHERE state IN ('preparing','implementing','awaiting_input','validating','integrating','releasing','blocked')
       OR EXISTS (SELECT 1 FROM sessions s WHERE s.id=r.session_id AND s.repository_claimed)
       OR EXISTS (SELECT 1 FROM issue_release_locks l WHERE l.run_id=r.id);

CREATE OR REPLACE FUNCTION guard_repository_claim() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND (NEW.repository_id, NEW.issue_run_id) IS DISTINCT FROM
        (OLD.repository_id, OLD.issue_run_id) THEN
        RAISE EXCEPTION 'Session repository and issue identity are immutable';
    END IF;
    IF NOT NEW.repository_claimed THEN RETURN NEW; END IF;
    IF TG_OP = 'UPDATE' AND OLD.repository_claimed AND
        NEW.working_directory = OLD.working_directory THEN RETURN NEW; END IF;
    PERFORM id FROM repositories WHERE id=NEW.repository_id FOR UPDATE;
    IF NEW.issue_run_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM issue_runs WHERE id=NEW.issue_run_id
          AND repository_id=NEW.repository_id AND state <> 'queued'
          AND (session_id IS NULL OR session_id=NEW.id)
    ) THEN
        RAISE EXCEPTION 'Session must belong to an admitted issue attempt';
    END IF;
    IF EXISTS (SELECT 1 FROM sessions WHERE repository_id=NEW.repository_id
        AND id <> NEW.id AND repository_claimed
        AND (NEW.issue_run_id IS NULL OR issue_run_id IS NULL)) OR
        (NEW.issue_run_id IS NULL AND (EXISTS (
            SELECT 1 FROM issue_runs WHERE repository_id=NEW.repository_id
              AND state IN ('preparing','implementing','awaiting_input','validating','integrating','releasing','blocked')
        ) OR EXISTS (
            SELECT 1 FROM issue_release_locks WHERE repository_id=NEW.repository_id
        ) OR EXISTS (
            SELECT 1 FROM issue_integrations i JOIN issue_runs r ON r.id=i.run_id
            WHERE i.repository_id=NEW.repository_id AND
                ((i.state <> 'merged' AND NOT i.retry_safe)
                  OR (i.merge_sha IS NOT NULL AND r.state <> 'released'))
        ))) THEN
        RAISE EXCEPTION 'Repository is busy; manual sessions require exclusive ownership'
            USING ERRCODE = '23505';
    END IF;
    RETURN NEW;
END;
$$;

-- The old missing-policy error happened before checks or external integration.
-- Make only those safely idle attempts conversational again; never replay work.
UPDATE issue_runs r SET workflow='repository',state='awaiting_input',
    validation_id=NULL,last_error=NULL,updated_at=now()
    FROM sessions s, issue_validations v
    WHERE r.session_id=s.id AND r.validation_id=v.id AND v.run_id=r.id
      AND r.state='blocked' AND r.validation_policy IS NULL
      AND v.state='blocked' AND v.policy IS NULL AND v.results='[]'::jsonb
      AND v.error='Validation setup blocked: No trusted build, test and lint policy is configured for this repository'
      AND s.status='idle' AND s.last_outcome='completed'
      AND NOT s.repository_claimed AND s.current_turn_id IS NULL AND s.channel_state='active'
      AND NOT EXISTS (SELECT 1 FROM issue_integrations WHERE run_id=r.id)
      AND NOT EXISTS (SELECT 1 FROM issue_releases WHERE run_id=r.id)
      AND NOT EXISTS (SELECT 1 FROM issue_release_locks WHERE run_id=r.id)
      AND NOT EXISTS (SELECT 1 FROM inbound_requests WHERE session_id=s.id AND state IN ('received','sending','uncertain'))
      AND NOT EXISTS (SELECT 1 FROM pending_requests WHERE session_id=s.id AND state IN ('pending','responding'));
UPDATE sessions s SET last_error=NULL,
    status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb('Ready for your instructions. Approve the repository workflow here; use /complete once it is finished.'::text)),
    updated_at=now()
    FROM issue_runs r WHERE r.session_id=s.id AND r.workflow='repository' AND r.state='awaiting_input';
INSERT INTO discord_outbox(dedupe_key,session_id,content,notify_requester)
    SELECT 'issue-input:' || r.id::text || ':workflow-upgrade:0', r.session_id,
        'This issue can now continue in the same conversation and worktree. Reply here to approve the repository workflow or request changes. No build/test/lint policy is required. Use /complete after the requested workflow is finished.', true
    FROM issue_runs r WHERE r.workflow='repository' AND r.state='awaiting_input'
    ON CONFLICT DO NOTHING;
