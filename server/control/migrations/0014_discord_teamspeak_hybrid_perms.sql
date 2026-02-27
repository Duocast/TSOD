-- Discord + TeamSpeak hybrid permission model primitives.

ALTER TABLE roles
  ADD COLUMN IF NOT EXISTS role_position INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS is_everyone BOOLEAN NOT NULL DEFAULT FALSE;

-- Ensure the seeded @everyone role is marked correctly.
UPDATE roles
SET is_everyone = TRUE
WHERE id = 'member';

-- Per-channel role-level overrides (tri-state via presence: inherit/allow/deny).
CREATE TABLE IF NOT EXISTS channel_role_overrides (
  channel_id    UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  role_id       TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  cap           TEXT NOT NULL,
  effect        TEXT NOT NULL CHECK (effect IN ('grant', 'deny')),
  PRIMARY KEY (channel_id, role_id, cap, effect)
);

CREATE INDEX IF NOT EXISTS idx_channel_role_overrides_channel_cap
  ON channel_role_overrides(channel_id, cap);
