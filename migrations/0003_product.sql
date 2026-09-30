-- Product metadata is deliberately outside immutable secret payload/policy records.
DO $$
DECLARE name text;
BEGIN
  FOREACH name IN ARRAY ARRAY['private_metadata','contact_states','invitation_states',
    'draft_sessions','deletion_requests','operation_receipts','account_preferences']
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
    EXECUTE format('CREATE INDEX ON %I (scope_id,id)', name);
    EXECUTE format('CREATE INDEX ON %I (due_at) WHERE due_at IS NOT NULL', name);
  END LOOP;
END $$;
CREATE INDEX receipt_actor_time ON operation_receipts ((data->>'actor_id'), ((data->>'at')::bigint) DESC);
CREATE INDEX deletion_request_expiry ON deletion_requests (((data->>'expires_at')::bigint));

-- Keep existing builder selections while reserving dialogs for sensitive prompts.
INSERT INTO draft_sessions(id,scope_id,data)
SELECT id,scope_id,jsonb_build_object('id',id,'dialog',data)
FROM dialogs WHERE data->>'draft_id' IS NOT NULL;
DELETE FROM dialogs WHERE data->>'draft_id' IS NOT NULL;
