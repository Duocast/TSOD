-- Ensure idempotent bootstrap upserts have proper unique constraints.

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1
    FROM pg_constraint
    WHERE conname = 'uq_user_roles_server_user_role'
  ) THEN
    ALTER TABLE user_roles
      ADD CONSTRAINT uq_user_roles_server_user_role UNIQUE (server_id, user_id, role_id);
  END IF;
END $$;

WITH ranked AS (
  SELECT ctid,
         ROW_NUMBER() OVER (
           PARTITION BY server_id, role_id, cap
           ORDER BY (effect = 'grant') DESC, ctid DESC
         ) AS rn
  FROM role_caps
)
DELETE FROM role_caps rc
USING ranked r
WHERE rc.ctid = r.ctid
  AND r.rn > 1;

CREATE UNIQUE INDEX IF NOT EXISTS uq_role_caps_server_role_cap
  ON role_caps(server_id, role_id, cap);
