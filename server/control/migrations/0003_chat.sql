-- Chat message persistence
CREATE TABLE IF NOT EXISTS chat_messages (
  id             UUID PRIMARY KEY,
  server_id      UUID NOT NULL,
  channel_id     UUID NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  author_user_id UUID NOT NULL,
  text           TEXT NOT NULL,
  attachments    JSONB NOT NULL DEFAULT '[]'::jsonb,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_chat_messages_channel_time
  ON chat_messages (channel_id, created_at DESC);
