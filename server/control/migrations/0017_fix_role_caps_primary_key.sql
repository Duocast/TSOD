-- Normalize role_caps uniqueness to a single row per (server_id, role_id, cap).

-- Ensure allowed is populated before removing legacy shape.
UPDATE role_caps
SET allowed = (effect = 'grant')
WHERE allowed IS NULL;

-- Keep newest row per (server_id, role_id, cap).
WITH ranked AS (
  SELECT ctid,
         ROW_NUMBER() OVER (
           PARTITION BY server_id, role_id, cap
           ORDER BY ctid DESC
         ) AS rn
  FROM role_caps
)
DELETE FROM role_caps rc
USING ranked r
WHERE rc.ctid = r.ctid
  AND r.rn > 1;

-- Remove previous key shape and promote server-scoped key.
ALTER TABLE role_caps DROP CONSTRAINT IF EXISTS role_caps_pkey;
DROP INDEX IF EXISTS uq_role_caps_server_role_cap;

-- Legacy role_caps effect no longer needed; allowed is the source of truth.
ALTER TABLE role_caps DROP COLUMN IF EXISTS effect;

ALTER TABLE role_caps
  ADD CONSTRAINT role_caps_pkey PRIMARY KEY (server_id, role_id, cap);
