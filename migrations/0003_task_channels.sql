-- Keep legacy thread IDs and all execution/approval records intact.
ALTER TABLE sessions
    ADD COLUMN discord_channel_id text,
    ADD COLUMN channel_kind text NOT NULL DEFAULT 'thread' CHECK (channel_kind IN ('thread','text')),
    ADD COLUMN channel_state text NOT NULL DEFAULT 'legacy' CHECK (channel_state IN ('legacy','provisioning','active','completed','dismissed','missing','deleted')),
    ADD COLUMN task_title text NOT NULL DEFAULT '',
    ADD COLUMN status_message_id text,
    ADD COLUMN status_snapshot jsonb NOT NULL DEFAULT '{}',
    ADD COLUMN status_revision bigint NOT NULL DEFAULT 0,
    ADD COLUMN status_delivered_revision bigint NOT NULL DEFAULT -1,
    ADD COLUMN discord_checked_at timestamptz,
    ADD COLUMN discord_retry_at timestamptz NOT NULL DEFAULT now(),
    ADD COLUMN discord_failures integer NOT NULL DEFAULT 0,
    ADD COLUMN discord_error text,
    ADD COLUMN completed_at timestamptz,
    ADD COLUMN delete_after timestamptz;

UPDATE sessions SET discord_channel_id=discord_thread_id;
CREATE UNIQUE INDEX one_session_per_discord_channel
    ON sessions (COALESCE(discord_channel_id, discord_thread_id));
CREATE INDEX task_channel_cleanup ON sessions(delete_after)
    WHERE channel_kind='text' AND channel_state IN ('completed','dismissed');

-- Execution updates also dirty the display. Presentation bookkeeping does not.
CREATE FUNCTION dirty_session_status() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF ROW(NEW.status, NEW.last_outcome, NEW.last_error, NEW.channel_state,
           NEW.task_title, NEW.status_snapshot, NEW.completed_at, NEW.delete_after)
       IS DISTINCT FROM
       ROW(OLD.status, OLD.last_outcome, OLD.last_error, OLD.channel_state,
           OLD.task_title, OLD.status_snapshot, OLD.completed_at, OLD.delete_after) THEN
        NEW.status_revision := OLD.status_revision + 1;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER session_status_changed BEFORE UPDATE ON sessions
    FOR EACH ROW EXECUTE FUNCTION dirty_session_status();

ALTER TABLE discord_outbox ADD COLUMN attachment text;
