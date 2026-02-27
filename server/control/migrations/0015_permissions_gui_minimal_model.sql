-- Minimal/hybrid permissions schema refinements for GUI support.

-- Roles: add GUI-facing metadata.
ALTER TABLE roles
  ADD COLUMN IF NOT EXISTS color INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS position INTEGER,
  ADD COLUMN IF NOT EXISTS is_system BOOLEAN NOT NULL DEFAULT FALSE;

-- Assign unique positions per server (role_position may be 0 for all
-- pre-existing rows, which would violate the unique index below).
UPDATE roles
SET position = sub.rn
FROM (
  SELECT id,
         ROW_NUMBER() OVER (
           PARTITION BY server_id
           ORDER BY role_position DESC, created_at
         ) - 1 AS rn
  FROM roles
  WHERE position IS NULL
) sub
WHERE roles.id = sub.id;

ALTER TABLE roles
  ALTER COLUMN position SET NOT NULL;

-- Scoped unique ordering per server.
CREATE UNIQUE INDEX IF NOT EXISTS uq_roles_server_position
  ON roles(server_id, position);

-- Role capabilities: server-scoped boolean model.
ALTER TABLE role_caps
  ADD COLUMN IF NOT EXISTS server_id UUID,
  ADD COLUMN IF NOT EXISTS allowed BOOLEAN;

UPDATE role_caps rc
SET server_id = r.server_id
FROM roles r
WHERE r.id = rc.role_id
  AND rc.server_id IS NULL;

UPDATE role_caps
SET allowed = (effect = 'grant')
WHERE allowed IS NULL;

ALTER TABLE role_caps
  ALTER COLUMN server_id SET NOT NULL,
  ALTER COLUMN allowed SET NOT NULL;

CREATE INDEX IF NOT EXISTS idx_role_caps_server_role_cap
  ON role_caps(server_id, role_id, cap);

-- Channel overrides split by principal and server scope; tri-state effect.
CREATE TABLE IF NOT EXISTS channel_user_overrides (
  server_id    UUID NOT NULL,
  channel_id   UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id      UUID NOT NULL,
  cap          TEXT NOT NULL,
  effect       TEXT NOT NULL CHECK (effect IN ('inherit', 'grant', 'deny')),
  PRIMARY KEY (server_id, channel_id, user_id, cap)
);

ALTER TABLE channel_role_overrides
  ADD COLUMN IF NOT EXISTS server_id UUID;

UPDATE channel_role_overrides cro
SET server_id = c.server_id
FROM channels c
WHERE c.id = cro.channel_id
  AND cro.server_id IS NULL;

ALTER TABLE channel_role_overrides
  ALTER COLUMN server_id SET NOT NULL;

ALTER TABLE channel_role_overrides
  DROP CONSTRAINT IF EXISTS channel_role_overrides_effect_check;
ALTER TABLE channel_role_overrides
  ADD CONSTRAINT channel_role_overrides_effect_check CHECK (effect IN ('inherit', 'grant', 'deny'));

CREATE UNIQUE INDEX IF NOT EXISTS uq_channel_role_overrides_v2
  ON channel_role_overrides(server_id, channel_id, role_id, cap);

INSERT INTO channel_user_overrides (server_id, channel_id, user_id, cap, effect)
SELECT c.server_id, co.channel_id, co.user_id, co.cap,
       CASE WHEN co.effect IN ('grant', 'deny') THEN co.effect ELSE 'inherit' END
FROM channel_overrides co
JOIN channels c ON c.id = co.channel_id
ON CONFLICT (server_id, channel_id, user_id, cap) DO UPDATE
SET effect = EXCLUDED.effect;

-- Audit payload naming compatibility for GUI.
ALTER TABLE audit_log
  ADD COLUMN IF NOT EXISTS payload_json JSONB;

UPDATE audit_log
SET payload_json = context_json
WHERE payload_json IS NULL;
