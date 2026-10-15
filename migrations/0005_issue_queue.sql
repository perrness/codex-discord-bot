CREATE TABLE issue_queues (
    repository_id uuid PRIMARY KEY REFERENCES repositories(id),
    paused boolean NOT NULL DEFAULT true,
    requester_id text NOT NULL,
    guild_id text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE issue_runs (
    id uuid PRIMARY KEY,
    repository_id uuid NOT NULL REFERENCES issue_queues(repository_id),
    github_repository text NOT NULL,
    issue_number bigint NOT NULL CHECK (issue_number > 0),
    attempt integer NOT NULL DEFAULT 1 CHECK (attempt > 0),
    state text NOT NULL DEFAULT 'queued' CHECK (state IN
        ('queued','preparing','implementing','validating','integrating','releasing','released','blocked','failed','cancelled')),
    claim_id uuid,
    claimed_at timestamptz,
    session_id uuid UNIQUE REFERENCES sessions(id),
    branch text,
    worktree text,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(repository_id, github_repository, issue_number, attempt)
);
-- Blocked claims retain the worker slot until an explicit, safe retry.
CREATE UNIQUE INDEX one_issue_worker_per_repository ON issue_runs(repository_id)
    WHERE state IN ('preparing','implementing','validating','integrating','releasing','blocked');
CREATE UNIQUE INDEX one_live_issue_attempt ON issue_runs(repository_id,github_repository,issue_number)
    WHERE state NOT IN ('failed','cancelled');
CREATE VIEW issue_execution AS SELECT r.*, s.codex_thread_id,
    COALESCE(s.discord_channel_id,s.discord_thread_id) AS discord_channel_id
    FROM issue_runs r LEFT JOIN sessions s ON s.id=r.session_id;
