-- Admission is serialized on this singleton row across all repository queues.
CREATE TABLE issue_worker_limits (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    global_limit integer NOT NULL DEFAULT 1 CHECK (global_limit BETWEEN 1 AND 32),
    repository_limit integer NOT NULL DEFAULT 1 CHECK (repository_limit BETWEEN 1 AND 32)
);
INSERT INTO issue_worker_limits DEFAULT VALUES;
DROP INDEX one_issue_worker_per_repository;

-- Only managed issue attempts may share a repository. Manual sessions remain
-- exclusive, including while an admitted issue has not allocated its session.
ALTER TABLE sessions ADD COLUMN issue_run_id uuid UNIQUE REFERENCES issue_runs(id);
UPDATE sessions s SET issue_run_id=r.id FROM issue_runs r WHERE r.session_id=s.id;
DROP INDEX one_executor_per_repository;
CREATE UNIQUE INDEX one_executor_per_worktree ON sessions(working_directory)
    WHERE repository_claimed;

CREATE FUNCTION guard_repository_claim() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND (NEW.repository_id, NEW.issue_run_id) IS DISTINCT FROM
        (OLD.repository_id, OLD.issue_run_id) THEN
        RAISE EXCEPTION 'Session repository and issue identity are immutable';
    END IF;
    IF NOT NEW.repository_claimed THEN RETURN NEW; END IF;
    IF TG_OP = 'UPDATE' AND OLD.repository_claimed AND
        NEW.working_directory = OLD.working_directory THEN RETURN NEW; END IF;
    -- Same row/order as queue admission. READ COMMITTED observes the previous
    -- claimant after this lock; a count without this lock would race.
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
              AND state IN ('preparing','implementing','validating','integrating','releasing','blocked')
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
CREATE TRIGGER repository_claim_guard BEFORE INSERT OR UPDATE OF repository_claimed,
    repository_id, issue_run_id, working_directory ON sessions
    FOR EACH ROW EXECUTE FUNCTION guard_repository_claim();

ALTER TABLE issue_runs ADD COLUMN integration_ready_at timestamptz;
ALTER TABLE issue_runs ADD COLUMN next_dispatch_at timestamptz NOT NULL DEFAULT now();
UPDATE issue_runs SET integration_ready_at=updated_at WHERE state='integrating';

-- This durable lock spans candidate validation, publication, merge AND release.
-- Restart and ambiguous remote responses cannot release it by dropping a task.
CREATE TABLE issue_release_locks (
    repository_id uuid PRIMARY KEY REFERENCES repositories(id),
    run_id uuid NOT NULL UNIQUE REFERENCES issue_runs(id),
    github_repository text NOT NULL
);
CREATE UNIQUE INDEX one_release_per_github_repository ON issue_release_locks(lower(github_repository));
INSERT INTO issue_release_locks(repository_id,run_id,github_repository)
    SELECT i.repository_id,i.run_id,r.github_repository FROM issue_integrations i JOIN issue_runs r ON r.id=i.run_id
    WHERE (i.state <> 'merged' AND NOT i.retry_safe)
       OR (i.merge_sha IS NOT NULL AND r.state <> 'released');

CREATE VIEW issue_worker_slots AS SELECT r.* FROM issue_runs r
    WHERE state IN ('preparing','implementing','validating','integrating','releasing','blocked')
       OR EXISTS (SELECT 1 FROM sessions s WHERE s.id=r.session_id AND s.repository_claimed)
       OR EXISTS (SELECT 1 FROM issue_release_locks l WHERE l.run_id=r.id);
