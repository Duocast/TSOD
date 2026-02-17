CREATE INDEX IF NOT EXISTS idx_channels_server_id ON channels(server_id);
CREATE INDEX IF NOT EXISTS idx_members_channel ON channel_members(channel_id);
CREATE INDEX IF NOT EXISTS idx_user_roles_user ON user_roles(server_id, user_id);
CREATE INDEX IF NOT EXISTS idx_outbox_unpublished ON outbox_events(server_id, published_at) WHERE published_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_audit_server_time ON audit_log(server_id, created_at DESC);
