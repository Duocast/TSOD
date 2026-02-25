-- Compatibility for legacy audit_log.context during transition to context_json.
-- Some deployments still have context as NOT NULL without a default, while new code
-- only populated context_json. Make legacy column safe for omitted inserts.

ALTER TABLE audit_log
  ALTER COLUMN context SET DEFAULT '{}'::jsonb,
  ALTER COLUMN context DROP NOT NULL;

-- Keep legacy context readable by backfilling from context_json where missing.
UPDATE audit_log
SET context = COALESCE(context_json, '{}'::jsonb)
WHERE context IS NULL;
