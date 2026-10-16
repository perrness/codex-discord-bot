ALTER TABLE sessions DROP CONSTRAINT sessions_status_check;
ALTER TABLE sessions ADD CONSTRAINT sessions_status_check CHECK (status IN
    ('provisioning','idle','running','waiting','stopping','recovering','unavailable','validating','integrating'));

-- Intent precedes remote mutations. Timeouts never authorize blind replay.
CREATE TABLE issue_integrations (
    run_id uuid PRIMARY KEY REFERENCES issue_runs(id),
    repository_id uuid NOT NULL REFERENCES repositories(id),
    validation_id uuid NOT NULL REFERENCES issue_validations(id),
    policy jsonb NOT NULL,
    validation_policy jsonb NOT NULL,
    state text NOT NULL DEFAULT 'preparing' CHECK (state IN
        ('preparing','validating','validated','pushing','pushed','creating_pr',
         'checking','merging','merged','blocked','uncertain')),
    branch text NOT NULL UNIQUE,
    worktree text NOT NULL UNIQUE,
    default_branch text,
    source_sha text,
    base_sha text,
    candidate_sha text,
    candidate_tree text,
    results jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(results)='array'),
    validated_at timestamptz,
    pr_number bigint CHECK (pr_number > 0),
    pr_url text,
    merge_request_id uuid,
    operation text CHECK (operation IN ('push','create_pr','merge')),
    merge_sha text,
    retry_safe boolean NOT NULL DEFAULT false,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (state <> 'merged' OR (merge_sha IS NOT NULL AND pr_number IS NOT NULL AND validated_at IS NOT NULL))
);
CREATE UNIQUE INDEX one_integration_per_repository ON issue_integrations(repository_id)
    WHERE state NOT IN ('merged','blocked');

CREATE VIEW integrated_issue_runs AS
    SELECT r.id FROM issue_runs r JOIN issue_integrations i ON i.run_id=r.id
    WHERE i.state='merged' AND i.merge_sha IS NOT NULL AND i.validated_at IS NOT NULL
      AND i.validation_id=r.validation_id AND r.id IN (SELECT id FROM validated_issue_runs)
      AND jsonb_array_length(i.results)=3
      AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(i.results) result
          WHERE result->>'outcome' IS DISTINCT FROM 'passed'
             OR result->>'exit_code' IS DISTINCT FROM '0')
      AND i.results @> '[{"name":"build"},{"name":"test"},{"name":"lint"}]';
