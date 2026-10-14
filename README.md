# Discord Codex Agent Manager

A native Rust service that connects a private Discord server to one local Codex App Server. Each task gets a Discord thread and a durable Codex conversation. Tokio, Serenity, SQLx/PostgreSQL, Serde, and tracing are used directly; there is no additional API server, container, Redis, or tmux orchestration.

The checkout in this development environment is `/root/repos/codex-discord-bot`. Deployment examples use your requested `/repos/codex-discord-bot` and `REPOS_ROOT=/repos`. Set absolute paths for your VM; neither location is hard-coded in the executable.

## Use from Discord

```text
/repo add name:fantasyai
/repo list
/project create name:my-new-project
/codex repo:fantasyai task:Implement player statistics
/sessions
```

Inside the new task thread, send normal text to continue. During execution it steers the active turn; when idle it starts another turn in the same conversation. Use `/status` or `/stop` within that thread. `/stop` requests cancellation; the repository remains reserved until Codex reports a terminal outcome and background terminals have stopped.

Ordinary agent questions accept normal messages. Structured questions have an **Answer questions** modal; a direct reply to a single-question prompt also works. Permission requests have separate approval/decline/cancel buttons. Command and file approvals apply to that request; **Allow this turn** grants a displayed permission profile for the current turn. Expired, already answered, cross-thread, or pre-restart buttons cannot authorize an operation.

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
3. Install the application in your private guild using the `bot` and `applications.commands` scopes. Give it **View Channel**, **Send Messages**, **Read Message History**, **Create Public Threads**, **Send Messages in Threads**, and **Manage Threads** in the task channel. Administrator permission is unnecessary.
4. Create a normal guild text channel visible only to your intended audience. The bot creates public threads within that private channel; it does not create forum posts or independently private threads.
5. Enable Discord Developer Mode to copy your user, guild, and channel IDs. Configure `ALLOWED_USER_IDS`, `DISCORD_GUILD_ID`, and `DISCORD_CHANNEL_ID`. Guild ID is optional, but recommended for this single-server setup. DMs are always rejected.

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

Session status follows `provisioning → running ↔ waiting → idle`, with `stopping` for cancellation and `recovering`/`unavailable` for failures. Completion outcome is stored separately as `completed`, `failed`, `interrupted`, or `unknown`. A database partial unique index and per-repository `flock` enforce the single-writer policy.

The service kills its App Server on critical database/protocol failure, retaining durable claims. Systemd cleans descendants and restarts it. Startup expires old interactive requests, resumes stored conversations where supported, checks the last turn and background terminals, and only then releases verified claims. It never automatically replays an initial task, follow-up, or approval whose delivery is uncertain. Send a new message after reviewing the recovered state to continue.

Completed agent messages and concise lifecycle updates are stored in an outbox. Long messages are split safely; Serenity handles Discord rate limits and failed sends retry with backoff. Reconnect catch-up fetches bounded message history from known task threads, using durable input receipts to reject duplicates. Attachments and message edits are not ingested. A very large offline backlog may require explicit user follow-up; this is not a general Discord archive service.

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
| Commands work but text does not | Message Content Intent and Read Message History; verify the message is in a mapped task thread. |
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
