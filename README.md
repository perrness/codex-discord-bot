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

Use `/complete` only after the task, validation, and any required integration/deployment are finished. A successful Codex turn alone leaves the task active. `/complete` updates the status, sends a completion notification, and moves the channel to the completed category. `/dismiss` explicitly closes a stopped or failed task. Both schedule deletion after `DISCORD_TASK_RETENTION_HOURS` (default **48**). Replying before deletion reopens the same conversation and cancels cleanup. Failed, waiting, interrupted, and unverified tasks otherwise stay active.

`/sessions` lists channel links and session IDs. From the control channel, use `/status`, `/status session:<id>`, `/stop session:<id>`, `/complete session:<id>`, or `/dismiss session:<id>` without navigating into the task channel. `/reconnect session:<id>` restores a manually deleted task channel without starting or replaying a Codex turn. The control channel receives system errors, not routine task progress. GitHub Issues queues are not implemented in this checkout.

For an idle legacy thread, `/dismiss session:<id>` hides it from `/sessions` while preserving its Discord thread, Codex history, and stored records. It does not schedule deletion. Pending approvals, uncertain input, and queued messages must be resolved first. Replying in the thread restores it to the list and continues the same conversation.

Ordinary agent questions accept normal messages. Structured questions have an **Answer questions** modal; a direct reply to a single-question prompt also works. The default permission mode is **Approve for me**: Codex automatically reviews eligible approval requests. `/status` shows the configured mode. Requests that still require your decision show the reason, working directory, command or file diff, and requested access, with supported approval/decline/cancel buttons. Command approvals apply to that request; file approvals also display any requested session write access. **Allow this turn** grants a displayed permission profile for the current turn. Expired, already answered, cross-thread, or pre-restart buttons cannot authorize an operation.

One modifying turn at a time is allowed per repository, including while waiting for approval or recovering. Conversations in other repositories can run concurrently. Multiple idle conversations may refer to the same repository; the first one to acquire a claim can continue. There are no automatic worktrees.

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

cargo fmt --all -- --check
```

Tests cover path traversal/symlinks, exclusive project creation, authorization, Unicode message limits, request IDs and out-of-order RPC responses, transport timeout/disconnection, database uniqueness, cross-session routing, approval correlation, stale events, duplicate inputs, and restart without replay after a lost turn-start response.

Channel tests also cover naming/collisions, coalesced status edits, completion and reopening, retention guards, missing channels/messages, interrupted creation, legacy migration, control-channel discovery/permissions, and Discord failures/retries. They use a small localhost HTTP fixture with the actual Serenity client and an isolated PostgreSQL schema; no live Discord guild is required.

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
