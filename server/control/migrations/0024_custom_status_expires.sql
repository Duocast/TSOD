-- Add custom_status_expires column to track auto-clear time for custom statuses.
ALTER TABLE user_profiles ADD COLUMN IF NOT EXISTS custom_status_expires TIMESTAMPTZ NULL;

-- Index for efficient querying of expired statuses during periodic cleanup.
CREATE INDEX IF NOT EXISTS idx_user_profiles_status_expires
    ON user_profiles (custom_status_expires)
    WHERE custom_status_expires IS NOT NULL;
