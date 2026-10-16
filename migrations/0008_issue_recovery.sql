-- Stable local and remote identities must not belong to two attempts.
CREATE UNIQUE INDEX issue_run_branch ON issue_runs(branch) WHERE branch IS NOT NULL;
CREATE UNIQUE INDEX issue_run_worktree ON issue_runs(worktree) WHERE worktree IS NOT NULL;
CREATE UNIQUE INDEX integration_pull_request ON issue_integrations(lower(policy->>'github_repository'), pr_number)
    WHERE pr_number IS NOT NULL;
CREATE UNIQUE INDEX integration_merge ON issue_integrations(lower(policy->>'github_repository'), merge_sha)
    WHERE merge_sha IS NOT NULL;

ALTER TABLE issue_runs ADD COLUMN recovery_phase text;
ALTER TABLE issue_runs ADD COLUMN worktree_error text;

-- The current release boundary is an operator's /complete confirmation. This
-- receipt does not claim to observe a deployment or authorize dispatching one.
CREATE TABLE issue_release_confirmations (
    run_id uuid PRIMARY KEY REFERENCES issue_runs(id),
    github_repository text NOT NULL,
    merge_sha text NOT NULL CHECK (merge_sha ~ '^[0-9a-f]{40}$'),
    confirmed_by text NOT NULL,
    confirmed_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX confirmed_release_revision ON issue_release_confirmations(lower(github_repository), merge_sha);
CREATE VIEW confirmed_issue_releases AS
    SELECT r.id FROM issue_runs r
    JOIN issue_integrations i ON i.run_id=r.id
    JOIN issue_release_confirmations c ON c.run_id=r.id
    WHERE r.id IN (SELECT id FROM integrated_issue_runs)
      AND c.github_repository=r.github_repository AND c.merge_sha=i.merge_sha;

-- Removing a worktree and recording completion cannot share a transaction.
-- Recovery inspects both the path and Git registration before doing anything.
CREATE TABLE issue_worktree_cleanup (
    run_id uuid NOT NULL REFERENCES issue_release_confirmations(run_id),
    kind text NOT NULL CHECK (kind IN ('issue','integration')),
    path text NOT NULL UNIQUE,
    branch text NOT NULL UNIQUE,
    revision text NOT NULL CHECK (revision ~ '^[0-9a-f]{40}$'),
    state text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','removing','removed','blocked')),
    last_error text,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(run_id, kind)
);
