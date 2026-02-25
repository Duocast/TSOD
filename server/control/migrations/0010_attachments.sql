CREATE TABLE IF NOT EXISTS attachments (
    id UUID PRIMARY KEY,
    server_id UUID NOT NULL,
    channel_id UUID NOT NULL,
    uploader_user_id UUID NOT NULL,
    filename TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes BIGINT NOT NULL,
    storage_path TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_attachments_channel_created
    ON attachments(channel_id, created_at DESC);
