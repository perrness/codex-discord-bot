-- One durable dispatch intent per integrated issue. Never retry a mutation on
-- timeout/restart; recover its remote identity and observe the original run.
ALTER TABLE issue_runs ADD COLUMN release_requested boolean NOT NULL DEFAULT false;
CREATE TABLE issue_releases (
    run_id uuid PRIMARY KEY REFERENCES issue_integrations(run_id),
    github_repository text NOT NULL,
    expected_sha text NOT NULL CHECK (expected_sha ~ '^[0-9a-f]{40}$'),
    policy jsonb NOT NULL,
    state text NOT NULL DEFAULT 'dispatching' CHECK (state IN
        ('dispatching','deploying','verifying','uncertain','failed','succeeded')),
    workflow_run_id bigint CHECK (workflow_run_id > 0),
    workflow_observation jsonb NOT NULL DEFAULT '{}',
    health_results jsonb NOT NULL DEFAULT '[]',
    deployed_revision text,
    deployment_deadline timestamptz NOT NULL,
    health_deadline timestamptz,
    verified_at timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (state <> 'succeeded' OR (workflow_run_id IS NOT NULL
        AND deployed_revision=expected_sha AND verified_at IS NOT NULL))
);
CREATE UNIQUE INDEX release_workflow_run ON issue_releases(lower(github_repository),workflow_run_id);
CREATE UNIQUE INDEX release_expected_revision ON issue_releases(lower(github_repository),expected_sha);

-- Preserve historical manual receipts without treating them as health evidence
-- for new controller-managed releases.
ALTER TABLE issue_release_confirmations ADD COLUMN verification text NOT NULL DEFAULT 'legacy_manual'
    CHECK (verification IN ('legacy_manual','controller'));
CREATE OR REPLACE VIEW confirmed_issue_releases AS
    SELECT r.id FROM issue_runs r
    JOIN issue_integrations i ON i.run_id=r.id
    JOIN issue_release_confirmations c ON c.run_id=r.id
    WHERE r.id IN (SELECT id FROM integrated_issue_runs)
      AND c.github_repository=r.github_repository AND c.merge_sha=i.merge_sha
      AND ((c.verification='legacy_manual' AND NOT EXISTS
            (SELECT 1 FROM issue_releases d WHERE d.run_id=r.id))
        OR (c.verification='controller' AND EXISTS
            (SELECT 1 FROM issue_releases d WHERE d.run_id=r.id
                AND d.state='succeeded' AND d.expected_sha=i.merge_sha
                AND d.deployed_revision=i.merge_sha AND d.verified_at IS NOT NULL)));
