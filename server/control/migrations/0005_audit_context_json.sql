-- Align audit_log schema with repository/model expecting `context_json`.
-- Older schemas created `context` JSONB; this migration adds/backfills `context_json`.

ALTER TABLE audit_log
  ADD COLUMN IF NOT EXISTS context_json JSONB;

DO $$
BEGIN
  IF EXISTS (
    SELECT 1
    FROM information_schema.columns
    WHERE table_schema = 'public'
      AND table_name = 'audit_log'
      AND column_name = 'context'
  ) THEN
    EXECUTE $stmt$
      UPDATE audit_log
      SET context_json = COALESCE(context, '{}'::jsonb)
      WHERE context_json IS NULL
    $stmt$;
  END IF;
END
$$;

UPDATE audit_log
SET context_json = '{}'::jsonb
WHERE context_json IS NULL;

ALTER TABLE audit_log
  ALTER COLUMN context_json SET DEFAULT '{}'::jsonb,
  ALTER COLUMN context_json SET NOT NULL;
