# Discord Codex Agent Manager

A native Rust service that connects a private Discord server to one local Codex App Server. Each new task gets a dedicated Discord text channel and a durable Codex conversation. Tokio, Serenity, SQLx/PostgreSQL, Serde, and tracing are used directly; there is no additional API server, container, Redis, or tmux orchestration.

The checkout and deployment examples use `/repos/codex-discord-bot` and `REPOS_ROOT=/repos`. Set absolute paths for your VM; the checkout location is not hard-coded in the executable.

## Use from Discord

```text
/repo add name:fantasyai
/repo list
/project create name:my-new-project
/codex repo:fantasyai task:Implement player statistics
/sessions
```

Start tasks from `#codex-control`. New channels such as `#fantasyai-implement-player-statistics` appear under the active category; name collisions get a checked session suffix. Inside a task channel, send normal text to continue. During execution it steers the active turn; when idle it starts another turn in the same conversation. `/stop` requests cancellation; the repository remains reserved until Codex reports a terminal outcome and background terminals have stopped.

Use `/complete` for manual tasks only after the task, validation, and any required integration/deployment are finished. A successful Codex turn alone leaves the task active. Queued issues complete automatically only after the release controller independently verifies the deployed revision and health; `/complete` cannot supply that evidence. Completion updates the status, sends a notification, and moves the channel to the completed category. `/dismiss` explicitly closes a safely stopped, unmerged task. Both schedule deletion after `DISCORD_TASK_RETENTION_HOURS` (default **48**). Replying before deletion reopens eligible manual conversations and cancels cleanup. Failed, waiting, interrupted, and unverified tasks otherwise stay active.

`/sessions` lists channel links and session IDs. From the control channel, use `/status`, `/status session:<id>`, `/stop session:<id>`, `/complete session:<id>`, or `/dismiss session:<id>` without navigating into the task channel. `/reconnect session:<id>` restores a manually deleted task channel without starting or replaying a Codex turn. The control channel receives system errors, not routine task progress. Use `/queue list repo:<name>` to view the read-only GitHub Issues backlog.

For an idle legacy thread, `/dismiss session:<id>` hides it from `/sessions` while preserving its Discord thread, Codex history, and stored records. It does not schedule deletion. Pending approvals, uncertain input, and queued messages must be resolved first. Replying in the thread restores it to the list and continues the same conversation.

Ordinary agent questions accept normal messages. Structured questions have an **Answer questions** modal; a direct reply to a single-question prompt also works. The default permission mode is **Approve for me**: Codex automatically reviews eligible approval requests. `/status` shows the configured mode. Requests that still require your decision show the reason, working directory, command or file diff, and requested access, with supported approval/decline/cancel buttons. Command approvals apply to that request; file approvals also display any requested session write access. **Allow this turn** grants a displayed permission profile for the current turn. Expired, already answered, cross-thread, or pre-restart buttons cannot authorize an operation.

Manual sessions allow one modifying turn per repository, including while waiting for approval or recovering, and exclude queued issue workers. Conversations in other repositories can run concurrently. Multiple idle manual conversations may refer to the same repository; the first one to acquire a claim can continue. Queued GitHub issues receive a fresh branch and worktree per attempt and can run concurrently within configured limits; stopped issues require `/queue retry` instead of a new turn in the old attempt. Manual sessions keep using the registered checkout.

Repository names must match `[A-Za-z0-9][A-Za-z0-9_-]{0,63}`. Registration requires a Git repository with a real `.git` directory directly under the configured root. Traversal, symlinked repository paths, symlinked Git metadata, and linked worktrees are rejected. Project creation is exclusive and preserves existing directories. If registration fails after creation, fix PostgreSQL and use `/repo add`; the created files are retained.

## Prepare the VM

Use Linux, PostgreSQL 14 or newer, Git, a current stable Rust toolchain supporting edition 2024 (manifest minimum 1.89), and the existing **Codex CLI 0.162.1**. The release binary is built with `Cargo.lock`. Only the CLI protocol version above is accepted; see [the protocol contract](docs/protocol.md) for upgrade checks.

Run the service under a non-root development account. The example unit uses `codexbot`; you can replace its `User`, `Group`, and `HOME` with your existing non-root account. The application refuses root in `run` mode. The account needs:

- Read/write access to the intended repositories and permission to create direct children of `/repos` for `/project create`.
- Access to the Codex executable, its persistent authentication directory, compilers, package managers, and project toolchains.
- A private state directory outside `/repos`. Parent directories must be trusted and must not be renamed or replaced while tasks run.

The service account should not have passwordless sudo. The bot validates repository selection, while Codex enforces execution sandboxing. This is a personal tool sharing a Unix account, not an isolation boundary between hostile tenants. Manual commands that bypass its lock can still modify the same files.

For a dedicated account, an administrator can create it and grant access only to the intended repositories. Avoid recursively changing ownership of all existing projects. Use your existing group/ACL arrangement where appropriate. An executable inside `/root` is normally inaccessible to this account; use an existing installation under its home or a shared installation path with traversable parent directories.

### Codex authentication and tools

As the chosen service account, with exactly the configured `CODEX_HOME`:

```sh
export CODEX_HOME=/home/codexbot/.codex
export CODEX_BIN=/home/codexbot/.local/bin/codex
"$CODEX_BIN" --version
"$CODEX_BIN" login --device-auth
"$CODEX_BIN" login status
```

Existing authentication can be reused by selecting its directory. Do not copy credentials into this repository or PostgreSQL. Keep `CODEX_HOME` private and outside `/repos`, and back it up with appropriate protection; it also holds Codex conversation history. Authentication setup is performed over SSH, never in Discord.

`TOOL_PATH` is an explicit list of absolute directories. Systemd does not source `.bashrc`, activate nvm, or expand `~` and `$HOME` in the environment file. Include the actual Rust/Node/Python tool locations your projects need. The App Server child receives `PATH`, `CODEX_HOME`, selected home/locale/CA variables, and no Discord token or database URL. Project-specific environment should use the service user's Codex configuration or project tooling; do not disable the sandbox to work around missing tools.

Verify that the installed Codex Linux sandbox works as this account, including any required sandbox helpers and kernel/AppArmor support. The supplied unit uses `NoNewPrivileges=true`; a sandbox setup that depends on a setuid helper needs an administrator-supported configuration compatible with that restriction. The protocol smoke test verifies policy negotiation, not execution of a sandboxed model task.

### Model and reasoning effort

Set `CODEX_MODEL=gpt-6-astra` and `CODEX_REASONING_EFFORT=max` in `/etc/codex-discord-bot.env` to use Astra at max effort. Supported effort levels depend on the selected model and installed Codex version. Empty values leave the corresponding Codex setting unchanged. Explicit settings apply to new and resumed conversations, including follow-ups in existing Discord task channels; the bot verifies the effective settings before submitting work. Restart the service after changing its environment, preferably after active tasks finish. No database migration is needed.

### Approval mode

`CODEX_APPROVALS_REVIEWER=auto_review` selects **Approve for me** and is the default when the variable is absent, including on upgrades. Codex's [automatic reviewer](https://learn.chatgpt.com/docs/sandboxing/auto-review) evaluates eligible requests and can approve or deny them. The bot uses `on-request` approval policy with the workspace-write sandbox and restricted network access. Any request Codex sends to Discord still requires your click; the bot does not blindly accept requests.

Set `CODEX_APPROVALS_REVIEWER=user` in the service environment file for **Ask for approval**. Only `auto_review` and `user` are supported; invalid values fail startup. The selected reviewer is applied to the App Server process, new conversations, resumed conversations, and every new turn, overriding the service user's Codex default. Rebuild and install the binary after upgrading, then restart the service to apply the mode. Restart expires pending approvals and interrupts active work; send a new message in the task channel or legacy thread to continue after recovery.

### Reuse PostgreSQL in Docker Compose

The Rust service connects from the host using `DATABASE_URL`. Use your existing Compose PostgreSQL service and a dedicated database/role. If its port is not already published, publish it on **127.0.0.1**, for example `127.0.0.1:5432:5432`; a Compose-only hostname such as `postgres` usually does not resolve from the host. Do not create another PostgreSQL container for the bot.

Using your existing administrator connection in `psql`:

```sql
CREATE ROLE codex_bot LOGIN;
\password codex_bot
CREATE DATABASE codex_bot OWNER codex_bot;
```

`\password` prompts without putting the password in shell history. Set `DATABASE_URL` in the protected environment file, with URL-escaped credentials as needed. This role owns the application's tables and SQLx migration metadata; it does not need superuser access. The application creates five tables: `repositories`, `sessions`, `inbound_requests`, `pending_requests`, and `discord_outbox`. SQLx adds its migration metadata table. Back up this database and Codex history together.

## Discord application setup

1. Create an application and bot in the [Discord Developer Portal](https://discord.com/developers/applications). Keep the bot private and store its token only in the environment file.
2. Enable the **Message Content Intent** on the Bot page. Natural conversation and reconnect catch-up require it. Member and presence intents are not used.
3. Install the application in your private guild using the `bot` and `applications.commands` scopes. Grant **Manage Channels** at guild scope. In both task categories and the control channel, grant **View Channel**, **Send Messages**, **Read Message History**, **Manage Channels**, **Manage Messages**, **Embed Links**, and **Attach Files**. Administrator permission is unnecessary. **Pin Messages** is optional; the bot attempts to pin each status message and `/status` also links to it. See Discord's [channel API](https://github.com/discord/discord-api-docs/blob/main/developers/resources/guild.mdx) and [message permissions](https://github.com/discord/discord-api-docs/blob/main/developers/resources/message.mdx).
4. Create private categories named `CODEX` and `COMPLETED`, visible to your intended audience. Set `DISCORD_ACTIVE_CATEGORY_ID` and `DISCORD_COMPLETED_CATEGORY_ID`. New task and control channels inherit the active category's complete permission overwrites by omitting explicit overwrites from the create request; the bot checks the result before using the channel. This avoids attempting to rewrite permissions the bot cannot grant (including administrator-only `MANAGE_ROLES` overwrites). Moves preserve existing visibility. Startup checks the category types, guild, and effective bot permissions. Presentation maintenance retries permission/transient failures without discarding execution state; failed creation requires explicit `/reconnect`.
5. Enable Discord Developer Mode to copy your user, guild, channel, and category IDs. Configure `ALLOWED_USER_IDS` and preferably `DISCORD_GUILD_ID` (otherwise inferred from the categories). `DISCORD_CONTROL_CHANNEL_ID` optionally selects an existing control channel. If omitted, the bot reuses a unique `#codex-control` in the guild or creates it in the active category; ambiguous names require an explicit ID. `DISCORD_CHANNEL_ID` remains a supported alias, so an existing control channel need not be renamed. DMs are always rejected.

For upgrades, retain **Send Messages in Threads** and **Manage Threads** on the old parent channel until all legacy sessions are retired. Migration `0003_task_channels.sql` preserves those sessions and approval records; it does not convert, move, or delete existing threads. Applying the migration does not interrupt running sessions. Installing a new binary still requires the normal service restart and recovery process.

Slash commands register at startup. A configured guild gets guild commands; otherwise commands register globally and can take longer to appear. The allowlist is checked on commands, thread messages, autocomplete, buttons, and modal submissions. Other bots and webhook messages are ignored. Messages from authorized users are visible to everyone who can view the channel, so channel permissions still matter. The bot suppresses mention notifications in generated messages.

## Build, migrate, install

From the checkout as a development user:

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

As an administrator, after creating/selecting the service account:

```sh
install -m 0755 target/release/codex-discord-bot /usr/local/bin/codex-discord-bot
install -m 0755 scripts/codex-locked /usr/local/bin/codex-locked
install -m 0600 .env.example /etc/codex-discord-bot.env
install -m 0644 deploy/codex-discord-bot.service /etc/systemd/system/codex-discord-bot.service
```

Edit `/etc/codex-discord-bot.env` with real values; it is not loaded automatically from the checkout. Adjust the unit's account and `HOME` to match. Its `StateDirectory`, `WorkingDirectory`, and configured `STATE_DIR` must identify the same directory. Both `STATE_DIR` and `CODEX_HOME` must be canonical, without symlink components, and outside `REPOS_ROOT`. Systemd creates the private state directory before launch.

Migrations are embedded and applied automatically on startup. To apply them separately, run `codex-discord-bot migrate` with only `DATABASE_URL` supplied through a protected environment. It does not require Discord or Codex credentials. For the root-owned example environment file, an administrator can use:

```sh
systemd-run --wait --pipe --collect --unit=codex-discord-migrate \
  --property=User=codexbot \
  --property=EnvironmentFile=/etc/codex-discord-bot.env \
  /usr/local/bin/codex-discord-bot migrate
systemctl daemon-reload
systemctl enable --now codex-discord-bot
journalctl -u codex-discord-bot -f
```

The migration command runs as the application user. No `sqlx-cli`, container mounts, or application Docker image are needed. Keep `KillMode=control-group`: the restart safety model depends on systemd cleaning the complete process tree. Start PostgreSQL before enabling the service; after repeated startup failures, fix the cause and run `systemctl reset-failed codex-discord-bot` followed by `systemctl start codex-discord-bot`.

Try a small `/codex` task in a disposable repository, then a follow-up, a question, a permission request, and `/stop`. Actual model execution requires working Codex authentication, network access for the App Server, and the sandbox prerequisites. This checkout's automated tests do not connect to a live Discord guild or execute a billable model turn.

## Manual SSH and tmux sessions

SSH and tmux remain available for manual development. To cooperate with the bot's per-repository lock, run under the same Unix account with the same `STATE_DIR`:

```sh
export CODEX_BIN=/home/codexbot/.local/bin/codex
export CODEX_HOME=/home/codexbot/.codex
export REPOS_ROOT=/repos
export STATE_DIR=/var/lib/codex-discord-bot
codex-locked fantasyai
```

The wrapper exits with code 75 if the repository is busy. It holds the same advisory filesystem lock for the manual CLI's lifetime. Keep running commands inside that lifetime. A manual session started without this wrapper, an editor, or an independently launched background process cannot be detected reliably by the bot. Do not use those to modify a repository while a bot task owns it. The bot itself never uses tmux.

## State, restart, and delivery

Session status follows `provisioning → running ↔ waiting → idle`, with `stopping` for cancellation and `recovering`/`unavailable` for failures. Turn outcome is stored separately as `completed`, `failed`, `interrupted`, or `unknown`. Discord channel lifecycle and task completion are independent of these execution states. A database partial unique index and per-repository `flock` enforce the single-writer policy.

The service kills its App Server on critical database/protocol failure, retaining durable claims. Systemd cleans descendants and restarts it. Startup expires old interactive requests, resumes stored conversations where supported, checks the last turn and background terminals, and only then releases verified claims. It never automatically replays an initial task, follow-up, or approval whose delivery is uncertain. Send a new message after reviewing the recovered state to continue.

Each task channel has one durable, editable status message. It shows execution state, the latest activity, and only the plan steps actually reported by Codex. Status edits coalesce at a five-second minimum interval; token fragments, reasoning, and individual tool events are not published. Explicit final answers, questions, approvals, errors, and completion notices use the durable outbox. Models without answer-phase metadata have their latest response held until the turn ends. Long ordinary output gets a UTF-16-safe preview and a text attachment; approval previews retain complete fenced chunks before their controls. Serenity handles rate limits, and failures use bounded backoff.

Restart reconciliation checks stored channel IDs and uses a session marker in each bot-created channel's topic to recover creation that succeeded before its database commit. Status-message recovery searches for its bot-authored session footer before creating a replacement. Keep these markers intact. A confirmed deleted channel is marked missing and reported in the control channel; Codex state and repository claims remain intact. Permission and transient API failures do not mean a channel was deleted. Use `/reconnect` to restore a missing channel explicitly; it never replays execution or approval responses.

Channel failures log `operation`, `http_status`, `discord_code`, `session_id`, and the Discord error message, with credentials redacted and no request URLs/headers/bodies. `/status session:<id>` retains the original diagnostic across restarts. HTTP 403 / Discord 50013 means a rejected operation, not an interrupted create; HTTP 5xx or a lost response is uncertain and must be reconciled by its session marker. `/reconnect session:<id>` checks an existing channel or searches for a unique marker before creating one; a failed lookup or duplicate markers stops creation. If setup failed before a Codex conversation existed, reconnect restores the channel and waits for a **new task message**. It never resubmits the original `/codex` input. Alternatively, `/dismiss session:<id>` archives a failed setup once its channel is confirmed absent. No schema migration, Administrator permission, or new Manage Roles grant is needed for this recovery fix.

Cleanup runs only for bot-owned completed/dismissed text channels with persisted completion metadata, after retention expires. It checks repository/turn state, outstanding input and approvals, status delivery, queued notifications, and unread authorized messages before deleting. The control channel and legacy threads are excluded. A per-session lock serializes cleanup with new input and approvals; deletion failures retain the session and retry. PostgreSQL keeps execution/recovery metadata and the outgoing queue, not a copy of Discord message history.

Reconnect catch-up fetches bounded message history from known task channels and legacy threads, using durable input receipts to reject duplicates. Incoming attachments and message edits are not ingested. A very large offline backlog may require explicit user follow-up; this is not a general Discord archive service.

Discord display delivery is at least once: a crash between a successful send and its database commit can duplicate a message once Discord's nonce window expires. Task execution is not retried by this delivery mechanism. If Discord remains unreachable, agents can continue until they finish, request input, or fail; results remain queued. Pending requests expire after one hour and cancel their turn.

## Logging and troubleshooting

```sh
systemctl status codex-discord-bot
journalctl -u codex-discord-bot --since '30 minutes ago'
systemctl restart codex-discord-bot
```

Tracing emits structured JSON. Configuration secrets and model token streams are not logged. Codex stderr activity is logged with content redacted by default. Temporarily set `CODEX_LOG_STDERR=1` and restart to capture actual App Server errors; these logs may contain project data, so restrict journal access and turn it off afterward.

| Symptom | Check |
| --- | --- |
| No commands | Guild/application installation, command registration log, channel permissions, allowlist. |
| Commands work but text does not | Message Content Intent and Read Message History; verify the message is in a mapped task channel or legacy thread. |
| Bot exits on startup | CLI version, service account permissions, canonical paths, database URL, and `codex login status` with the same account/home. |
| Missing compiler or Git credentials | Literal `TOOL_PATH`, service-user home, and project configuration; an SSH login environment is not inherited. |
| Approval button disabled | Missing/oversized preview, unsupported permission scope, expired request, or completed turn. Narrow the task or inspect via SSH. |
| Repository stays reserved | `/sessions` and `/status`; inspect failed cleanup, a manual lock, or an unavailable Codex history. Do not simply clear a live claim. |
| Replies are delayed | `/status` shows queued output; check Discord permissions, archived/locked threads, rate limits, and connectivity. |
| Restart loop | Stop the service, fix PostgreSQL/authentication/configuration, then reset the systemd failure limit. |

For a persistently `unavailable` session, stop the service and inspect the unit's cgroup, running manual processes, repository changes, and Codex history. Restore the missing executable/home/path/authentication first and restart; recovery retries claimed sessions. If history was permanently lost, only after confirming all writers are stopped may you release that specific session with an administrative update setting `repository_claimed=false` and `current_turn_id=NULL` while leaving `status='unavailable'`. Then start a **new** task. There is deliberately no Discord force-unlock button.

## Tests and source layout

Default tests require only Git and no Discord or database connection. Database tests require an explicitly supplied disposable PostgreSQL database/role with `CREATE SCHEMA`; each uses a unique schema and cleans it on success. A failed test can leave a `codex_test_*` schema for inspection.

```sh
# Set TEST_DATABASE_URL securely to a disposable test database first.
cargo test --test integration --locked -- --include-ignored

# Uses a temporary empty Codex home: no credentials and no model turn.
CODEX_TEST_BIN=/absolute/path/to/codex \
  cargo test --test codex_smoke --locked -- --include-ignored

# On a Linux test host that supports bubblewrap and unprivileged user namespaces.
# Use a root-owned, secret-free image with /bin/sh, coreutils and util-linux.
TEST_VALIDATION_ROOTFS=/opt/codex-validation/test-rootfs \
  cargo test --test validation_sandbox --locked -- --include-ignored

cargo fmt --all -- --check
```

Tests cover path traversal/symlinks, exclusive project creation, authorization, Unicode message limits, request IDs and out-of-order RPC responses, transport timeout/disconnection, database uniqueness, cross-session routing, approval correlation, stale events, duplicate inputs, and restart without replay after a lost turn-start response.

Channel tests also cover naming/collisions, coalesced status edits, completion and reopening, retention guards, missing channels/messages, interrupted creation, legacy migration, control-channel discovery/permissions, and Discord failures/retries. They use a small localhost HTTP fixture with the actual Serenity client and an isolated PostgreSQL schema; no live Discord guild is required.

Validation tests cover exit codes, bounded diagnostics, timeouts and descendant cleanup, missing policies, pinned repair limits, persisted evidence, completion guards and restart without replay. The optional sandbox test additionally exercises actual namespace and filesystem isolation; a host that cannot create the sandbox must fail this check, without an unsandboxed fallback.

| Path | Responsibility |
| --- | --- |
| `src/config.rs` | Environment configuration, allowlist, private directory boundaries. |
| `src/repositories.rs` | Repository names, filesystem validation, Git initialization. |
| `src/codex/` | Typed protocol subset, transport, process ownership, effective policy validation. |
| `src/sessions.rs`, `src/sessions/` | Turn lifecycle, per-session ordering, approval handling, recovery. |
| `src/discord/` | Commands, messages, buttons/modals, rendering, durable delivery. |
| `src/persistence.rs`, `migrations/` | PostgreSQL state and constraints. |
| `deploy/`, `scripts/` | Systemd unit and optional manual-session lock. |

See [protocol details and supported limitations](docs/protocol.md) before changing the CLI version, approval policy, or restart behavior.

Task channels mention the requester for approvals, structured questions, failures/interruption, explicit task completion/dismissal, and recovery or expired-request notices. Routine progress stays quiet. Only the requester is allowed to be pinged; mentions embedded in model output cannot notify other users or roles.

## GitHub Issues backlog

Install the GitHub CLI (`gh`) on the service PATH. As the service account, run `gh auth login --hostname github.com` and `gh auth status`; grant read access to issues in the intended repositories (including private repositories when needed). The bot reuses that account’s CLI authentication; never submit credentials through Discord. `GH_TOKEN` is also supported by gh when provided securely in the service environment. Discovery does not change GitHub state or start Codex.

Discovery validates `origin` as an HTTPS or SSH github.com URL. For repositories without a suitable origin, use `/repo add name:project github:owner/repository`; this explicit mapping is persisted in the existing repository record and overrides origin. Existing registrations continue to work without a mapping; a missing or unsupported origin produces a queue error. Migration `0004_github_repository.sql` adds the optional mapping column and runs on startup.

Create the `codex-ready` label in each GitHub repository and apply it to open issues intended for the backlog. Issues with any assignee or the `codex-claimed` label are considered claimed and excluded. Pull requests and closed issues are excluded. Queue dispatch is opt-in through `/queue start`. Issue descriptions returned by the REST API are discarded by gh before the bot parses results and are not passed to Codex by discovery; queued tasks receive the issue URL to inspect requirements. Displayed titles are untrusted and sanitized, with mention notifications suppressed.

`/queue list repo:project` returns issue numbers, titles, links and ready status. Large results show a bounded preview with a link to the complete filtered backlog. Discovery follows pages of 100 issues, with per-command and overall timeouts and output limits. Authentication, permission, rate-limit and pagination failures return an error instead of a misleading partial backlog. There are no automatic retries after rate-limit errors; check `gh auth status` and retry after the API limit resets.


### Durable issue queue

`/queue start repo:project` discovers the current ready backlog and persists it before enabling dispatch. Repeat start to discover new issues; existing issues are never automatically retried. `/queue pause` prevents new claims while active work finishes. `/queue resume` enables pending dispatch without rediscovery. `/queue status` shows current workers and recent attempts with their linked sessions; reports longer than one Discord message include the full details as a text attachment. All commands require `repo`.

`ISSUE_WORKERS_GLOBAL` and `ISSUE_WORKERS_PER_REPOSITORY` bound admitted issue workers across the service and within each registered repository. Both default to **1**, accept 1–32, and take effect on restart. For example, global 4 and repository 2 permit two issues in each of two repositories. Admission is transactional across scheduler ticks and repositories. Preparing, running, validating, integration-waiting, blocked and release-pending attempts retain slots; uncertain remote operations retain theirs even if a stale run was cancelled. Lowering limits lets existing work settle without cancelling it. `/queue status` shows used/configured slots, queued work, waiting candidates, the integration/release owner, and task-channel links.

Each issue has a separate branch, worktree, Codex conversation and Discord channel. Managed workers take shared repository file locks; manual checkout sessions and cleanup require exclusive locks. Database guards also prevent manual reservation from racing issue admission. While a worker is running, follow-ups steer its existing turn. A stopped issue requires explicit dismissal/retry so a new message cannot bypass admission limits. Codex workspace-write is not read isolation for the service account; elevated commands retain the existing approval policy.

Use the supplied systemd unit to enforce aggregate limits on Codex, Git, validation and all their descendants: `CPUQuota=200%`, `MemoryHigh=3G`, `MemoryMax=4G`, `TasksMax=512`, and `LimitNOFILE=8192`. Tune these for the VM with an operator-owned systemd drop-in before raising concurrency. Memory exhaustion stops the service and recovery preserves uncertain attempts. Worker counts also bound simultaneously running checks; each worker runs its checks sequentially. An unmanaged `cargo run` does not install these OS resource limits. Keep `KillMode=control-group` and `OOMPolicy=stop`.

Queue work uses the existing task channels and approval routing. After each successful issue turn, the Rust orchestrator runs trusted build, test and lint checks while retaining that worktree's claim. All three must exit successfully before the issue reaches `integrating`. Validated workers wait in completion order for a durable integration/release slot. Only its owner prepares and validates a fresh candidate and creates/merges a protected PR. A confirmed merge moves the issue to `releasing`; the release controller can then dispatch the configured trusted workflow. The slot remains held until workflow completion and independent revision health checks pass. A safely blocked, unmerged attempt relinquishes its integration slot and can be cancelled with `/dismiss`; uncertain publication, merge or release outcomes keep the slot. Different repositories can integrate independently; local aliases of the same GitHub repository share a release slot.

Express prerequisites using GitHub's native [blocked-by issue dependencies](https://docs.github.com/en/rest/issues/issue-dependencies#list-dependencies-an-issue-is-blocked-by). Before dispatch, all pages must confirm each dependency closed as completed; tracked dependencies must also have finished their managed release. Open, cancelled, malformed or inaccessible dependencies keep the issue queued, with a visible reason and a read-only recheck after 60 seconds. The scheduler can admit other ready issues while it waits. Dependencies are checked again after acquiring the integration slot and immediately before merge. A newly blocked dependency at integration stops the attempt for explicit review. Free-text dependency descriptions do not establish scheduler dependencies; cycles or unresolved dependencies require operator correction.

Migration `0009_issue_concurrency.sql` replaces repository-wide worker uniqueness with bounded admission, unique working-directory ownership, immutable session/issue identity and manual-session exclusion. It also adds the integration/release lock, preserving outstanding release and uncertain-operation ownership during upgrade. No queued turn, validation, merge or release is replayed after restart.

Migration `0005_issue_queue.sql` stores identities, attempts, claims, state and branch/worktree fields. Thread and Discord channel IDs are exposed through `issue_execution` using the linked session. Claims have no automatic expiry: restart records the interrupted phase, blocks unfinished runs after session recovery, and reconciles external evidence without resubmitting a turn. Inspect the session and finish/dismiss it before `/queue retry repo:project issue:123`. Retry creates a new attempt and respects pause. Failed, blocked or cancelled attempts require explicit retry; released issues are never redispatched. Each attempt reserves a unique `codex/issue-<number>-attempt-<attempt>-<run UUID>` branch and `REPOS_ROOT/.worktrees/<repo>/<run UUID>` directory before creating the worktree from the registered checkout’s `HEAD`. The repository must have an initial commit. The fresh Codex thread and all follow-ups use that worktree; approvals retain the task channel routing. Recovery verifies the path, Git common directory and branch before resuming history. Partial allocations remain on disk for inspection and are never recreated automatically. Explicit retry allocates a new branch, worktree and conversation. Clean worktrees become eligible for automatic cleanup only after an operator confirms the release.

### Trusted issue validation

Set `VALIDATION_CONFIG` to a canonical, root-owned JSON file such as `/etc/codex-discord-bot-validation.json`. Copy [deploy/validation.example.json](deploy/validation.example.json), use registered local repository names as keys, and set explicit build, test and lint argument arrays. Executables use absolute paths inside the worker image; commands are never taken from GitHub issue content or configuration in a worktree. Each timeout must be 1–3600 seconds and `max_repairs` must be 0–5. The file and its parent directories must not be writable by the service account, group or others. Restart after operator configuration changes. Missing repository policies block queued integration; malformed configuration prevents startup. Manual tasks do not require this configuration.

Install **bubblewrap** at `/usr/bin/bwrap` with support for unprivileged user namespaces, `--disable-userns` and `--json-status-fd`. Provision a dedicated root filesystem, such as `/opt/codex-validation/rust-rootfs`, containing only the compiler, shell, libraries and offline dependencies needed by the trusted commands. It must have root-owned files/directories without group/other write permission, no sockets, devices or FIFOs, and trusted parent directories. It must not overlap repositories, service state, Codex credentials, `/etc`, `/home`, `/root` or `/run`. Do not use a copy of the host filesystem or include deployment secrets. Tools must run with `HOME=/tmp/home` and `PATH=/usr/local/bin:/usr/bin:/bin`; install standalone toolchains or root-owned wrappers in the image, with vendored dependencies or an image-local package cache. The example assumes `cargo` and its toolchain are available at `/usr/local/bin/cargo`; a service-account rustup installation is not mounted. The bot never installs packages or builds an image from issue-controlled files.

The worker mounts only that image read-only and the issue worktree at `/workspace`, with private `/tmp`, `/home`, `/run`, devices and `/proc`. It clears the inherited environment, isolates user/PID/network/IPC namespaces, drops capabilities, prevents privilege gain and nested user namespaces, and receives no service credentials, host Docker socket or host home directory. Worktrees containing sockets, devices or FIFOs are rejected. There is no unsandboxed fallback. Keep the service's `KillMode=control-group`; timeouts, `/stop` and shutdown kill the worker process group and its PID namespace. This isolates validation commands; ordinary Codex implementation turns still use the separately documented Codex approval and sandbox policy.

Migration `0006_issue_validation.sql` records the pinned policy, originating turn, bounded stdout/stderr (8 KiB per stream), exit codes, durations, timeouts and validation outcome in `issue_validations`. Check failures and timeouts are returned as diagnostics to the **same issue thread and worktree**, up to the pinned repair limit. Every repair reservation and input receipt commits before submission; an uncertain submission is never replayed. Worker/setup errors, cancellation, missing checks and exhausted repairs block integration and `/complete`. A subsequent modifying turn invalidates earlier evidence; after an integration attempt is reserved, further edits require a new issue attempt. Restart marks in-progress checks interrupted and unfinished issues blocked; it does not rerun checks or repairs. Inspect the results, then `/dismiss` and explicitly `/queue retry` when appropriate. The attempt's repair count cannot be increased by changing configuration midway through the attempt.

### Automatic issue integration

Set `INTEGRATION_CONFIG` to a canonical, root-owned JSON file outside repositories, with trusted parents and no group/other write permission, for example `/etc/codex-discord-bot-integration.json`. Start from [deploy/integration.example.json](deploy/integration.example.json). Each registered repository explicitly pins its GitHub `owner/repository`, which must match both the issue mapping and `origin`. No policy means no push or PR creation. `checks_timeout_seconds` (1–3600) bounds waiting for GitHub checks and for merge completion. Manual tasks do not invoke this pipeline.

Before enabling a repository, configure default-branch protection to **require branches to be up to date**, enforce protection for administrators, and require the `codex/integration` status. Enable merge commits. The service account needs contents and PR write, commit-status write, and administration **read** permissions to inspect protection. Other required checks and review rules remain enforced; if they cannot be satisfied automatically, integration blocks. The bot uses GitHub.com's [asynchronous merge API](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request-asynchronously) with an expected head SHA, `merge_method=merge`, `merge_action=direct_merge`, and `bypass_rules=false`. It does not change protection, force-push, auto-update a PR branch, or enqueue a merge queue. Rulesets-only repositories and merge-queue-only policies currently fail closed; configure compatible strict branch protection or integrate manually.

`release_gate` is a required **operator attestation**, not an automatic audit of external deployment systems. Use `deployments_disabled` only after disabling unattended deployment on every push/PR/merge trigger, including external webhooks. Use `dispatch_only` only after deployment requires the trusted release controller to explicitly select and verify the recorded merge SHA. CI must not expose deployment credentials to issue branches. List repository-controlled release scripts, manifests and other gate configuration in `protected_paths`; `.github` is always protected. Candidates changing these paths block before publication. Keep secrets and deployment permissions outside the Codex/validation worker. Without a matching release policy the merged issue remains blocked. A `main` push alone must not deploy.

After acquiring the integration/release slot, the orchestrator snapshots tracked and non-ignored implementation changes into a commit, fetches the current default branch and creates a separate `codex/integration-<run UUID>` branch/worktree merging that base with the issue commit. Each fetch pins its own temporary ref instead of reading the shared `FETCH_HEAD`. It reruns the pinned build, test and lint policy in the isolated worker against this exact candidate, including changes integrated while the worker was waiting. Conflicts, missing checks, source/worktree changes, and default/head advancement block the attempt. Only a passing immutable candidate is pushed, using an ordinary fast-forward push to its unique branch. The required status is posted for that SHA; strict branch protection guards base advancement between the last local observation and GitHub's merge. The final merge commit has new metadata, so the bot verifies its tree equals the validated candidate and its parents equal the pinned base and candidate before accepting its actual SHA for release.

Migration `0007_issue_integration.sql` stores the pinned policies, source/base/candidate/tree SHAs, per-check evidence, publication/merge intents, PR URL/number, async merge request ID and observed merge SHA. The unique integration record and durable release lock serialize integration and release. `/queue status` exposes the phase, revision and PR. GitHub failures are reconciled by reading the unique branch/PR, never blindly repeating push, create or merge. Uncertain operations are polled read-only once per minute. Restart performs only read-only GitHub reconciliation: a verified completed merge is adopted; incomplete checks and open/unknown operations block. A merged issue keeps its worker and release slots while awaiting release. `/stop` cancels checks or integration-slot waits, but an already submitted merge may still finish and must be reconciled.

After a failed attempt, inspect the preserved worktrees and PR. Close an unmerged PR before `/dismiss` and `/queue retry`; both commands reconcile before allowing reuse. Retry creates a new run, conversation, branch and worktree, never overwriting the old branch. Unknown PR creation with no observable PR remains blocked for operator inspection; after ruling out an in-flight request, an operator can mark that `issue_integrations` row `state='blocked', retry_safe=true`. Never clear an observed merge SHA or retry an issue awaiting release. Uncertain or mismatched merges retain their observed SHA and block release. No GitHub Issue content can change these policies.

### Crash recovery and worktree cleanup

Migration `0008_issue_recovery.sql` adds unique worktree, PR and merge identities, the interrupted phase and worktree diagnostics, release-confirmation receipts, and cleanup intents. Startup reconciles sessions first, then the queue, GitHub integration evidence and managed worktrees before dispatch is enabled. Repeated restarts retain claims and evidence; they do not create replacement attempts. `/queue status` shows the interrupted phase, errors, release confirmation and cleanup status. Recovery, blocked cleanup and explicit retries use durable, deduplicated Discord notices in the existing task channel. Runs whose channel was never created remain visible through `/queue status`.

| Interrupted boundary | Recovery behavior |
| --- | --- |
| Dispatch / worktree allocation | Keep the run blocked; inspect any allocated path and stored Codex conversation. Never allocate or submit again. |
| Validation / repair | Mark running checks interrupted, retain their results and consumed repair budget, and block integration. Never rerun checks or submit the reserved repair. |
| Integration | Read the persisted PR identity or unique branch. Adopt only a verified merge; open, absent-after-create, mismatched or unreadable remote state remains blocked. Never repeat push, PR creation or merge. |
| Release | Observe only the persisted workflow dispatch, then check the exact deployed revision independently. Missing dispatch intent blocks for explicit `/queue release`; never repeat deployment on restart. Verified health, the receipt, `released` state, cleanup intents and task completion commit atomically. |
| Worktree removal | Inspect both Git registration and the filesystem. Absence from both confirms a prior removal; partial removal or changed identity requires inspection. |

Historical manual release receipts remain readable. New queued releases require controller evidence; `/complete` no longer creates manual release receipts. Previously released runs without a receipt remain historical releases and are not automatically cleaned; migrations never fabricate a confirmation.

Startup and the minute maintenance pass clean only confirmed released attempts, under the session gate and repository file lock. Both implementation and integration worktrees must match their managed path, branch and recorded revision. Dirty, untracked, ignored, locked, missing-but-registered or otherwise unverifiable worktrees are preserved and reported. After inspection and removal of unwanted artifacts, the next maintenance pass rechecks cleanup. Git removal never uses force or broad pruning; branches and commits remain available. Failed, blocked and cancelled attempts are always preserved. Worktree cleanup is separate from Discord channel retention and never authorizes a release or retry.

### Automatic issue releases

Run the opt-in release controller with `codex-discord-bot releases` under a **separate non-root Unix account**, using [deploy/codex-discord-release.service](deploy/codex-discord-release.service). The bot process never dispatches workflows and never receives the controller's GitHub token. The controller loads only its own configuration, acquires a separate PostgreSQL leader lock and uses the shared durable outbox for Discord notifications; it does not spawn Codex or load a Discord token. Its credential-bearing home must be canonical, owned by that account and private (0700). `CODEX_WORKER_UID` is required and prevents launching the controller under the Codex account or root.

Create the dedicated `codex-release` OS account with no interactive login, apply bot migrations, create a separate PostgreSQL login and grant [deploy/release-grants.sql](deploy/release-grants.sql). Install [deploy/release.env.example](deploy/release.env.example) as `/etc/codex-discord-release.env`, root-owned mode 0600. Set `CODEX_WORKER_UID` to the numeric output of `id -u codexbot`, set the database login, and provide a dedicated GitHub token with Actions read/write and contents read. The Codex/bot GitHub identity must have **no Actions write or production environment access**. Keep the service accounts out of each other's groups, do not grant Codex sudo or host Docker access, and adapt the service's inaccessible paths if repository/state roots differ. Install the unit, reload systemd, and enable it only after the disposable deployment test below. Keep both services' channel retention settings consistent.

Set `RELEASE_CONFIG` in the **release service environment** to a canonical root-owned JSON file with trusted parents, outside repositories, starting from [deploy/release.example.json](deploy/release.example.json). No service/policy means no unattended deployment. Each repository must have a matching `dispatch_only` integration policy **before integrating issues**. This implementation supports existing GitHub Actions workflows; it does not run Docker Compose or deployment scripts on the Codex host. Install root-owned `/usr/bin/gh` and `/usr/bin/curl`. Production application credentials stay exclusively in the workflow's protected GitHub environment or remote deployment service; the dispatch token stays in the separate service account. Neither belongs in the bot environment, checkout, Codex home, validation image or a worker-accessible credential store.

Use a protected, immutable workflow tag/branch and pin its full commit in `workflow_sha` plus its numeric `workflow_id`. The integration/worker identity must not be able to change or bypass that protection. The controller verifies the ref before dispatch and the actual workflow source SHA afterward. Restrict the production environment to the trusted ref, block worker branches from deployment secrets, and disable all push-triggered deployments and external merge webhooks before enabling integration. A post-dispatch identity check cannot undo deployment by an unprotected ref that changed during dispatch.

Adapt the existing workflow to the contract in [deploy/release-workflow.example.yml](deploy/release-workflow.example.yml): accept string inputs `revision` and `release_id`, and set `run-name: 'codex-release:${{ inputs.release_id }}:${{ inputs.revision }}'`. The workflow source runs from `workflow_ref`; **the deployed application must use `inputs.revision`**, not the workflow's `github.sha`, a moving branch or `latest` image. Build the exact revision without production secrets, then deploy the immutable artifact in a separate protected job that uses only trusted deployment code. Await the actual rollout before the workflow succeeds. Serialize every deployment entry point with the same concurrency group and `cancel-in-progress: false`. The example intentionally fails until an operator supplies tested deployment jobs.

The controller uses GitHub's [workflow dispatch API](https://docs.github.com/en/rest/actions/workflows#create-a-workflow-dispatch-event), persists intent before the request, and records its returned run ID. An uncertain response is reconciled through complete, bounded workflow-run discovery using the durable UUID and revision in the run name. It never chooses the latest run or repeats dispatch. A different workflow, repository, source SHA, title or rerun attempt fails verification. Deployment waiting is bounded by `deployment_timeout_seconds` (1–7200); after workflow success, `health_timeout_seconds` (1–3600) bounds health polling. Deadlines survive restart.

Each of 1–8 configured HTTPS health URLs must independently return a 2xx JSON response such as `{"healthy":true,"revision":"<full 40-character deployed commit SHA>"}`. `revision` must come from the running artifact, and `healthy` must cover the actual application and its required dependencies/replicas. Do not reflect the requested revision or report only a load balancer's own health. Responses are bounded to 16 KiB, with 1–30 second per-request timeouts, no redirects, no proxy or curl configuration and no credentials. All checks must report the expected SHA in the same verification pass; missing, stale, malformed or unhealthy results cannot release the issue.

Before enabling a real application, exercise the same workflow revision against a disposable deployment, independently check its health endpoint, then test wrong revisions, unsuccessful rollout, lost responses, restart and health timeout. Record the successful run URL in `disposable_test_run`. This is an operator attestation that the disposable rollout and health procedure were tested; preflight also confirms that the recorded GitHub run succeeded with the configured workflow ID and source SHA. Updating the trusted workflow requires a new disposable test. The automated suite uses a disposable HTTP service and mocked Actions transport; PostgreSQL tests use disposable schemas and require `TEST_DATABASE_URL`.

Migration `0010_issue_releases.sql` persists release requests, the policy, expected SHA, workflow ID/observations, health results, deadlines and observed deployed revision. A live verified merge requests release in PostgreSQL; the separate controller atomically consumes that request before dispatching once. The existing repository/GitHub release lock spans integration through successful health verification. Failure or uncertainty keeps that lock and sends a durable Discord notice. Other repositories can continue releasing. `/queue status` shows the expected and deployed revisions, failed health checks and workflow link. An issue recovered without dispatch intent requires `/queue release repo:<name> issue:<number>` after inspection; missing policy never starts deployment. That command queues a request only if no intent exists; the controller observes existing intents automatically and never retries failed deployments or resets deadlines. `/queue retry`, `/dismiss` and `/complete` cannot bypass a failed or unverified release.

There is **no automatic rollback or database downgrade**. A failed deployment remains blocked for operator investigation; only use a separately configured, tested application rollback procedure with explicit migration/data compatibility checks. Rolling back to an earlier healthy SHA does not release the current issue. Recovery does not redispatch, rerun a workflow, or invent success after a timeout. Resolve failed deployments and their durable records under an operator runbook before permitting further releases; never simply clear the release lock or synthesize a health receipt.
