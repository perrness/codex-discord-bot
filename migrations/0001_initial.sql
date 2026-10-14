CREATE TABLE repositories (
    id uuid PRIMARY KEY,
    name text NOT NULL UNIQUE CHECK (name ~ '^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$'),
    path text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sessions (
    id uuid PRIMARY KEY,
    repository_id uuid NOT NULL REFERENCES repositories(id),
    requester_id text NOT NULL,
    guild_id text NOT NULL,
    parent_channel_id text NOT NULL,
    discord_thread_id text UNIQUE,
    codex_thread_id text UNIQUE,
    working_directory text NOT NULL,
    status text NOT NULL CHECK (status IN ('provisioning','idle','running','waiting','stopping','recovering','unavailable')),
    repository_claimed boolean NOT NULL DEFAULT true,
    current_turn_id text,
    last_outcome text CHECK (last_outcome IN ('completed','failed','interrupted','unknown')),
    last_error text,
    last_seen_message_id text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX one_executor_per_repository ON sessions(repository_id) WHERE repository_claimed;
CREATE INDEX recent_sessions ON sessions(updated_at DESC);

CREATE TABLE inbound_requests (
    discord_id text PRIMARY KEY,
    session_id uuid REFERENCES sessions(id),
    kind text NOT NULL,
    state text NOT NULL CHECK (state IN ('received','sending','accepted','uncertain','rejected')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE pending_requests (
    id uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES sessions(id),
    generation uuid NOT NULL,
    rpc_id jsonb NOT NULL CHECK (jsonb_typeof(rpc_id) IN ('string','number')),
    method text NOT NULL,
    turn_id text NOT NULL,
    item_id text,
    payload jsonb NOT NULL,
    state text NOT NULL CHECK (state IN ('pending','responding','resolved','expired')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (generation, rpc_id)
);

CREATE TABLE discord_outbox (
    id bigserial PRIMARY KEY,
    dedupe_key text NOT NULL UNIQUE,
    session_id uuid NOT NULL REFERENCES sessions(id),
    content text NOT NULL,
    request_id uuid REFERENCES pending_requests(id),
    discord_message_id text,
    controls_closed boolean NOT NULL DEFAULT false,
    attempts integer NOT NULL DEFAULT 0,
    delivered_at timestamptz,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX undelivered_messages ON discord_outbox(next_attempt_at, id) WHERE delivered_at IS NULL;
