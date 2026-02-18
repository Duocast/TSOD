-- Outbox claim fields for multi-gateway publication
ALTER TABLE outbox_events
  ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ NULL,
  ADD COLUMN IF NOT EXISTS claim_token TEXT NULL;

CREATE INDEX IF NOT EXISTS idx_outbox_unpublished_claimable
  ON outbox_events (server_id, created_at)
  WHERE published_at IS NULL;
