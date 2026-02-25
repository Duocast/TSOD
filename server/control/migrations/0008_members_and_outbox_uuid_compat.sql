-- Bring `members` table/schema in line with repository expectations.
-- Legacy schema used `channel_members` without server_id/updated_at.
CREATE TABLE IF NOT EXISTS members (
  server_id    UUID NOT NULL,
  channel_id   UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id      UUID NOT NULL,
  display_name TEXT NOT NULL,
  muted        BOOLEAN NOT NULL DEFAULT FALSE,
  deafened     BOOLEAN NOT NULL DEFAULT FALSE,
  joined_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (server_id, channel_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_members_server_channel ON members(server_id, channel_id);

DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.tables
    WHERE table_schema = 'public'
      AND table_name = 'channel_members'
  ) THEN
    EXECUTE $stmt$
      INSERT INTO members (server_id, channel_id, user_id, display_name, muted, deafened, joined_at, updated_at)
      SELECT c.server_id, cm.channel_id, cm.user_id, cm.display_name, cm.muted, cm.deafened, cm.joined_at, NOW()
      FROM channel_members cm
      JOIN channels c ON c.id = cm.channel_id
      ON CONFLICT (server_id, channel_id, user_id)
      DO UPDATE SET
        display_name = EXCLUDED.display_name,
        muted = EXCLUDED.muted,
        deafened = EXCLUDED.deafened,
        joined_at = LEAST(members.joined_at, EXCLUDED.joined_at),
        updated_at = NOW()
    $stmt$;
  END IF;
END
$$;

-- Outbox ID/claim token types must be UUID for repository decoding/binding.
-- Legacy schemas used TEXT id (ULID/comment) and TEXT claim_token.
DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'outbox_events'
      AND column_name = 'id'
      AND udt_name <> 'uuid'
  ) THEN
    EXECUTE 'ALTER TABLE outbox_events ADD COLUMN IF NOT EXISTS id_uuid UUID';

    -- Cast UUID strings directly; for non-UUID legacy IDs derive stable UUID from md5(id).
    EXECUTE $stmt$
      UPDATE outbox_events
      SET id_uuid = CASE
        WHEN id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'
          THEN id::uuid
        ELSE (
          substr(md5(id), 1, 8) || '-' ||
          substr(md5(id), 9, 4) || '-' ||
          '4' || substr(md5(id), 14, 3) || '-' ||
          'a' || substr(md5(id), 18, 3) || '-' ||
          substr(md5(id), 21, 12)
        )::uuid
      END
      WHERE id_uuid IS NULL
    $stmt$;

    EXECUTE 'ALTER TABLE outbox_events DROP CONSTRAINT IF EXISTS outbox_events_pkey';
    EXECUTE 'ALTER TABLE outbox_events DROP COLUMN id';
    EXECUTE 'ALTER TABLE outbox_events RENAME COLUMN id_uuid TO id';
    EXECUTE 'ALTER TABLE outbox_events ALTER COLUMN id SET NOT NULL';
    EXECUTE 'ALTER TABLE outbox_events ADD CONSTRAINT outbox_events_pkey PRIMARY KEY (id)';
  END IF;
END
$$;

DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'outbox_events'
      AND column_name = 'claim_token'
      AND udt_name <> 'uuid'
  ) THEN
    EXECUTE 'ALTER TABLE outbox_events ADD COLUMN IF NOT EXISTS claim_token_uuid UUID';

    EXECUTE $stmt$
      UPDATE outbox_events
      SET claim_token_uuid = CASE
        WHEN claim_token IS NULL OR claim_token = '' THEN NULL
        WHEN claim_token ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'
          THEN claim_token::uuid
        ELSE (
          substr(md5(claim_token), 1, 8) || '-' ||
          substr(md5(claim_token), 9, 4) || '-' ||
          '4' || substr(md5(claim_token), 14, 3) || '-' ||
          'a' || substr(md5(claim_token), 18, 3) || '-' ||
          substr(md5(claim_token), 21, 12)
        )::uuid
      END
      WHERE claim_token_uuid IS NULL
    $stmt$;

    EXECUTE 'ALTER TABLE outbox_events DROP COLUMN claim_token';
    EXECUTE 'ALTER TABLE outbox_events RENAME COLUMN claim_token_uuid TO claim_token';
  END IF;
END
$$;
