# Codex protocol contract

This implementation targets **codex-cli 0.162.1**. It refuses other versions, so an automatic CLI upgrade cannot silently change approval or recovery behavior. The local CLI's generated JSON schemas were inspected alongside the [official App Server documentation](https://developers.openai.com/codex/app-server). A real-executable smoke test checks negotiation and effective sandbox settings without credentials or model execution.

The documented App Server SDKs cover TypeScript and Python. This application uses a small Rust client for the documented wire protocol; it does not depend on unpublished Codex internal Rust crates or scrape terminal output.

To review a CLI upgrade in a temporary directory:

```sh
codex --version
codex app-server generate-json-schema --out /tmp/codex-schema
codex app-server generate-json-schema --experimental --out /tmp/codex-schema-experimental
```

Review changes, update the version constant, and rerun the contract and recovery tests before deployment. In this version `thread/start.sandbox` is the string `workspace-write`; `turn/start.sandboxPolicy.type` is `workspaceWrite`. They are different schema representations.

## Transport and authentication

The process starts with `codex app-server --listen stdio://`. One JSON object per line travels over private stdin/stdout. Messages omit the `jsonrpc` property. Requests and responses correlate by integer or string `id`; an incoming message with both `id` and `method` is a **server request**, not a response to a client request with the same ID.

The client sends `initialize` with `clientInfo` and `capabilities.experimentalApi=true`, awaits its response, then sends `initialized`. Experimental capability is needed for the background-terminal cleanup API used to release repository claims safely. There is no general negotiated protocol-version number; the executable pin and schema checks provide compatibility control.

`account/read` with `refreshToken:false` checks the existing account. Authentication stays in the service user's protected `CODEX_HOME`. Sign in through the installed CLI under that account; no browser login, API keys, or credential forms are handled in Discord. The child receives a reduced environment without the bot token or `DATABASE_URL`.

## Methods used

| Method | Purpose |
| --- | --- |
| `thread/start` | Durable conversation with explicit `cwd`, `sandbox`, `approvalPolicy`, and `approvalsReviewer`. |
| `thread/resume` | Reload a stored conversation with the same working directory and policy; `excludeTurns:true` limits response size. |
| `turn/start` | Text input with explicit working directory, workspace-write sandbox policy, and configured approval reviewer. |
| `turn/steer` | Follow-up during execution, guarded by `expectedTurnId`. |
| `turn/interrupt` | Request interruption; retain the repository claim until terminal state and process cleanup. |
| `thread/turns/list` | Inspect the latest turn and full items during startup reconciliation. |
| `thread/backgroundTerminals/clean` | Stop App Server background terminals before releasing a claim. |
| `thread/backgroundTerminals/list` | Verify cleanup; a missing/unknown result is not considered empty. |

The request's `clientUserMessageId` is a correlation field. This client does **not** assume it makes turn submission idempotent. A timed-out or disconnected mutation is never automatically repeated.

`thread/start` can precede creation of resumable on-disk history. A crash before the first turn may leave a conversation that cannot be resumed; recovery marks it unavailable rather than inventing a replacement task.

## Events and interactions

The reader separates RPC responses from notifications and server requests. Bounded queues dispatch events by Codex thread ID, with one ordered worker per mapped session. RPC responses remain readable while workers await database or protocol operations. Queue overflow ends the connection and triggers reconciliation instead of silently losing approvals or completion events.

`item/completed` supplies complete agent messages and plans. Commentary updates the persistent status; explicit final answers remain conversational messages. Unknown phases are coalesced until the terminal turn event. `turn/plan/updated` supplies actual step/status pairs (`pending`, `inProgress`, `completed`); no workflow stages are invented. These fields were verified against the pinned CLI's generated schema. Token, reasoning, and command-output deltas are not sent to Discord. `item/started` retains file-change previews for approval. `turn/completed` has distinct `completed`, `failed`, and `interrupted` outcomes; `error` may indicate a retry and is not itself a terminal outcome. A completed turn does not complete the overall task: authorized `/complete` confirms that required validation/integration/deployment is finished.

| Server request | Response and Discord behavior |
| --- | --- |
| `item/commandExecution/requestApproval` | Original RPC ID with `{ "decision": "accept" }`, `decline`, or `cancel`. Respect advertised acceptance/decline decisions. Show reason, exact command or terminal input, directory, and network/additional access; no session-wide acceptance or rule amendment. |
| `item/fileChange/requestApproval` | Same decision shape; acceptance requires complete file paths, change kinds, and diffs. Display moves and any requested `grantRoot` session write access. |
| `item/permissions/requestApproval` | `{ "permissions": <explicit grant>, "scope": "turn" }`; decline grants `{}`. Only understood fields and canonical paths within the task repository can be granted in Discord. Requests forwarded to Discord show network/file access and require a click. |
| `item/tool/requestUserInput` | `{ "answers": { "questionId": { "answers": ["text"] } } }`; one modal field per question, at most five. A Discord reply to a single-question prompt is also accepted. |
| `serverRequest/resolved` notification | Correlate the original request ID and process generation; remove controls and refresh waiting state. |

Normal prose questions use normal Discord messages. Protocol questions require correlation to the pending request. Text cannot approve an operation. Requests flagged as secret input are rejected before persistence; configure credentials over SSH. Approval previews omit protocol IDs, timestamps, duplicate command parsing, and unused rule-amendment proposals. Unfamiliar request details remain visible. Commands and diffs use code fences maintained across Discord message boundaries; controls appear only after the full preview. Oversized or incomplete approval previews disable acceptance, including permission requests. Unanswered requests expire after an hour and interrupt the turn.

Unknown interactions fail closed. MCP elicitation forwarded to this client is declined; custom tool calls and external authentication refresh flows are unsupported. `CODEX_APPROVALS_REVIEWER` selects `auto_review` (default, **Approve for me**) or `user` (**Ask for approval**). The process config and `thread/start`, `thread/resume`, and `turn/start` explicitly send the selected reviewer with `on-request`. Thread responses must confirm that reviewer, the expected working directory, and the existing workspace-write/network restrictions. Invalid reviewer configuration or a mismatched effective policy fails closed.

In `auto_review` mode, Codex's [automatic reviewer](https://learn.chatgpt.com/docs/sandboxing/auto-review) approves or denies eligible requests; the bot does not synthesize acceptance responses. Requests Codex still forwards to Discord retain the same human approval and correlation checks. Ordinary sandboxed operations may execute without an approval event in either mode. Automatic review does not disable the sandbox or guarantee approval. Workspace-write is not a read-isolation boundary for the whole Unix account. Review [Codex security](https://developers.openai.com/codex/security) before granting an unsandboxed command request.

## Restart semantics and limitations

The bot terminates its App Server process group on shutdown. The systemd unit owns the complete cgroup, cleans descendants, and restarts the application after critical failure. Keeping the unit's `KillMode=control-group` is part of the safety model; launching multiple unmanaged copies does not provide equivalent process cleanup. A local instance lock and PostgreSQL advisory lock reject duplicate deployments.

Startup invalidates old approvals, marks incomplete input receipts uncertain, and reconciles claimed sessions before accepting Discord input. It resumes history, verifies idle state, cleans terminals, and compares the latest terminal turn ID with the persisted current turn. An unmatched or unknown outcome remains **unknown**. It may recover missing final messages, but never starts another turn to finish interrupted work. An unverifiable session keeps its repository claim for operator inspection.

General multi-agent task tools are disabled to keep one Discord task channel (or legacy thread) mapped to one root conversation; Codex's internal automatic approval reviewer is configured separately. Incoming attachments, linked Git worktrees, MCP login forms, session-wide command approval decisions, and automatically continuing interrupted work are unsupported. Long outgoing responses may be attached as text files. The working directory is stored separately from the repository, allowing a future worktree allocator without changing Discord routing.

Discord and PostgreSQL cannot share a transaction. Durable receipts prevent replaying accepted or uncertain task submissions; the outbox retries presentation separately. Discord nonce deduplication only covers a short recent window, so a crash after a successful send but before its database commit can duplicate a displayed message. It does not replay the coding task.

New Discord text channels carry a durable session UUID in their topic; persistent status embeds carry a session footer. Recovery adopts matching objects after an interrupted database commit, rather than blindly recreating them. Unknown channel (Discord error 10003) and unknown message (10008) are distinguished from permission/transient failures. Missing channels never discard Codex execution state. Channel creation is attempted once per new task; an uncertain/unrecoverable setup needs explicit `/reconnect`, which creates or adopts presentation only. Legacy Discord thread IDs remain routable and are never eligible for automatic deletion. Channel completion and retention are independent of Codex turn status and repository ownership.

Task identity, title and text-channel provisioning state are reserved atomically. A rejected create (such as HTTP 403 / Discord 50013) records the operation and API error and marks the channel missing; a lost response or server error remains uncertain until a marker lookup. Startup never retries creation or replaces a recorded API failure with a generic ambiguity message. Channel binding and redisplay of still-pending prompts commit together, so a failed status send cannot strand an approval on an old channel. `/reconnect` preserves existing conversation IDs and execution claims. If no conversation ID was ever persisted, no turn could have been submitted: recovery rejects the unfinished input receipt, and explicit reconnect allows a new message to initialize the conversation under the usual repository locks. It does not replay the failed command. A failed setup with no channel or conversation can also be explicitly dismissed after absence is confirmed.

Explicitly dismissing an idle legacy thread marks its channel state `dismissed` and hides it from `/sessions`, with no deletion deadline. Its routing and execution/approval records remain intact. An authorized follow-up restores `legacy` state before continuing the same conversation through the normal input and repository-claim checks; dismissal alone never starts a turn.
