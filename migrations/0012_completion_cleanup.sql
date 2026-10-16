-- Pin the revision explicitly confirmed by /complete. Historical confirmations
-- have no recorded revision and are not automatically enrolled in cleanup.
ALTER TABLE issue_workflow_completions ADD COLUMN revision text
    CHECK (revision ~ '^[0-9a-f]{40}$');

-- Cleanup can follow either repository-workflow completion or a verified managed
-- release. The worker checks the corresponding receipt before removing anything.
ALTER TABLE issue_worktree_cleanup DROP CONSTRAINT issue_worktree_cleanup_run_id_fkey;
ALTER TABLE issue_worktree_cleanup ADD CONSTRAINT issue_worktree_cleanup_run_id_fkey
    FOREIGN KEY (run_id) REFERENCES issue_runs(id);
