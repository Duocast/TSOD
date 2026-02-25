-- Align outbox_events schema with repository/model expecting `payload_json`.
-- Legacy schemas use `payload` and may enforce NOT NULL without defaults.

ALTER TABLE outbox_events
  ADD COLUMN IF NOT EXISTS payload_json JSONB;

DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'outbox_events'
      AND column_name = 'payload'
  ) THEN
    EXECUTE $stmt$
      UPDATE outbox_events
      SET payload_json = COALESCE(payload, '{}'::jsonb)
      WHERE payload_json IS NULL
    $stmt$;
  END IF;
END
$$;

UPDATE outbox_events
SET payload_json = '{}'::jsonb
WHERE payload_json IS NULL;

ALTER TABLE outbox_events
  ALTER COLUMN payload_json SET DEFAULT '{}'::jsonb,
  ALTER COLUMN payload_json SET NOT NULL;

-- Compatibility for legacy `payload` during transition; keep old readers safe.
DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'outbox_events'
      AND column_name = 'payload'
  ) THEN
    EXECUTE $stmt$
      ALTER TABLE outbox_events
        ALTER COLUMN payload SET DEFAULT '{}'::jsonb,
        ALTER COLUMN payload DROP NOT NULL
    $stmt$;

    EXECUTE $stmt$
      UPDATE outbox_events
      SET payload = COALESCE(payload_json, '{}'::jsonb)
      WHERE payload IS NULL
    $stmt$;
  END IF;
END
$$;

-- Compatibility for legacy required `key` column that newer writes omit.
DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'outbox_events'
      AND column_name = 'key'
  ) THEN
    EXECUTE $stmt$
      ALTER TABLE outbox_events
        ALTER COLUMN key SET DEFAULT '',
        ALTER COLUMN key DROP NOT NULL
    $stmt$;

    EXECUTE $stmt$
      UPDATE outbox_events
      SET key = ''
      WHERE key IS NULL
    $stmt$;
  END IF;
END
$$;
