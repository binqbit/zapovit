-- Each business entity has a separate table. JSONB stores its versioned typed record;
-- relational identity, scheduling and uniqueness constraints remain database-owned.
DO $$
DECLARE name text;
BEGIN
  FOREACH name IN ARRAY ARRAY['accounts','profiles','plans','participants','invitations','drafts',
    'secret_versions','secret_guardians','release_cases','guardian_submissions','file_objects',
    'delivery_parts','recovery_claims','cancellation_requests','dialogs','actions','outbox','control_intents','handled_events','delivery_attempts','deletion_tombstones']
  LOOP
    EXECUTE format('CREATE TABLE %I (
      id uuid PRIMARY KEY,
      scope_id uuid,
      data jsonb NOT NULL CHECK (jsonb_typeof(data) = ''object''),
      due_at bigint GENERATED ALWAYS AS ((data->>''due_at'')::bigint) STORED,
      state text GENERATED ALWAYS AS (data->>''state'') STORED,
      updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
      CHECK ((data->>''id'')::uuid = id)
    )', name);
    EXECUTE format('CREATE INDEX ON %I (scope_id)',name);
    EXECUTE format('CREATE INDEX ON %I (due_at) WHERE due_at IS NOT NULL',name);
  END LOOP;
END $$;

CREATE UNIQUE INDEX account_telegram_identity ON accounts ((data->>'telegram_id'));
CREATE UNIQUE INDEX profile_owner ON profiles ((data->>'owner_id')) WHERE state <> 'deleted';
CREATE UNIQUE INDEX profile_recovery ON profiles ((data->>'recovery_selector')) WHERE state <> 'deleted';
CREATE UNIQUE INDEX plan_profile ON plans ((data->>'profile_id')) WHERE state <> 'deleted';
CREATE UNIQUE INDEX participant_binding ON participants ((data->>'plan_id'),(data->>'account_id'));
CREATE UNIQUE INDEX guardian_binding ON secret_guardians ((data->>'secret_id'),(data->>'account_id'));
CREATE UNIQUE INDEX submission_binding ON guardian_submissions ((data->>'case_id'),(data->>'account_id'));
CREATE UNIQUE INDEX one_live_release_case ON release_cases ((data->>'secret_id'))
  WHERE data#>>'{case,state}' IN ('collecting','waiting','ready','delivering','partial');
CREATE UNIQUE INDEX logical_part ON delivery_parts ((data->>'secret_id'),(data->>'recipient_id'),(data->>'index'));
CREATE UNIQUE INDEX object_key ON file_objects ((data->>'key'));
CREATE UNIQUE INDEX open_cancellation ON cancellation_requests ((data->>'plan_id'),COALESCE(data->>'secret_id','')) WHERE state = 'open';
CREATE INDEX owner_profiles ON profiles ((data->>'owner_id'));
CREATE INDEX ready_jobs ON outbox (state,due_at);
CREATE INDEX claim_profile ON recovery_claims ((data->>'profile_id'));

ALTER TABLE secret_versions ADD CONSTRAINT valid_threshold CHECK (
  (data#>>'{policy,threshold}')::integer BETWEEN 1 AND jsonb_array_length(data#>'{policy,guardians}')
  AND jsonb_array_length(data#>'{policy,guardians}') BETWEEN 1 AND 10
  AND jsonb_array_length(data#>'{policy,recipients}') BETWEEN 1 AND 10
);

CREATE TABLE rate_limits (
  key text PRIMARY KEY,
  window_start bigint NOT NULL,
  used bigint NOT NULL CHECK (used > 0)
);

CREATE TABLE telegram_cursor (
  singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  bot_id bigint NOT NULL,
  next_offset bigint NOT NULL DEFAULT 0,
  last_event_at timestamptz,
  last_poll_at timestamptz,
  last_scheduler_at timestamptz,
  hold_until bigint NOT NULL DEFAULT 0
);

CREATE TABLE telegram_inbox (
  bot_id bigint NOT NULL,
  update_id bigint NOT NULL,
  envelope jsonb,
  received_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  processed_at timestamptz,
  attempts integer NOT NULL DEFAULT 0,
  priority boolean NOT NULL DEFAULT false,
  PRIMARY KEY(bot_id,update_id)
);
CREATE INDEX pending_inbox ON telegram_inbox(update_id) WHERE processed_at IS NULL;
CREATE INDEX pending_inbox_priority ON telegram_inbox(priority DESC,update_id) WHERE processed_at IS NULL;

CREATE TABLE audit_events (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at timestamptz NOT NULL DEFAULT clock_timestamp(),
  operation text NOT NULL,
  target_id uuid,
  actor_id uuid,
  result text NOT NULL
);

ALTER TABLE deletion_tombstones ADD CONSTRAINT minimal_deletion_tombstone CHECK (
  scope_id IS NULL
  AND data->>'scope' IN ('secret','plan','profile')
  AND data ?& ARRAY['id','scope','journal_operation_id']
  AND data - ARRAY['id','scope','journal_operation_id'] = '{}'::jsonb
);

CREATE FUNCTION reject_deleted_scope() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM deletion_tombstones WHERE id=NEW.id) THEN
        RAISE EXCEPTION 'deleted scope cannot be recreated';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER never_restore_deleted_secret BEFORE INSERT OR UPDATE ON secret_versions
    FOR EACH ROW EXECUTE FUNCTION reject_deleted_scope();
CREATE TRIGGER never_restore_deleted_plan BEFORE INSERT OR UPDATE ON plans
    FOR EACH ROW EXECUTE FUNCTION reject_deleted_scope();
CREATE TRIGGER never_restore_deleted_profile BEFORE INSERT OR UPDATE ON profiles
    FOR EACH ROW EXECUTE FUNCTION reject_deleted_scope();

CREATE FUNCTION protect_secret_policy() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.data->'policy' IS DISTINCT FROM OLD.data->'policy' OR NEW.scope_id IS DISTINCT FROM OLD.scope_id THEN
        RAISE EXCEPTION 'immutable secret policy';
    END IF;
    IF NEW.data->'payload' IS DISTINCT FROM OLD.data->'payload' AND NEW.data->>'state' <> 'deleted' THEN
        RAISE EXCEPTION 'immutable secret payload';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER immutable_secret BEFORE UPDATE ON secret_versions FOR EACH ROW EXECUTE FUNCTION protect_secret_policy();

CREATE TABLE maintenance (
    singleton BOOLEAN PRIMARY KEY DEFAULT true CHECK(singleton),
    enabled BOOLEAN NOT NULL DEFAULT false,
    changed_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO maintenance(singleton,enabled) VALUES(true,false);
