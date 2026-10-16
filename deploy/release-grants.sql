-- Run as the schema owner after migrations, in the application's database.
-- Create the LOGIN role codex_release and set its password separately.
-- Adapt public if the bot uses a different schema. This role cannot migrate,
-- change integration/validation policy, or read Discord approval payloads.
GRANT USAGE ON SCHEMA public TO codex_release;
GRANT SELECT ON repositories, sessions, issue_runs, issue_integrations,
    issue_validations, issue_release_locks, issue_releases,
    issue_release_confirmations, validated_issue_runs, integrated_issue_runs,
    confirmed_issue_releases TO codex_release;
GRANT UPDATE ON sessions, issue_runs, issue_releases TO codex_release;
GRANT INSERT ON issue_releases, issue_release_confirmations,
    issue_worktree_cleanup, discord_outbox TO codex_release;
-- ON CONFLICT(dedupe_key) needs SELECT on its conflict target, not message data.
GRANT SELECT(dedupe_key) ON discord_outbox TO codex_release;
GRANT DELETE ON issue_release_locks TO codex_release;
GRANT USAGE ON SEQUENCE discord_outbox_id_seq TO codex_release;
