-- Enforce one active voice membership per user per server.
WITH ranked AS (
    SELECT
        ctid,
        ROW_NUMBER() OVER (
            PARTITION BY server_id, user_id
            ORDER BY updated_at DESC NULLS LAST, joined_at DESC NULLS LAST, channel_id ASC
        ) AS rn
    FROM members
)
DELETE FROM members m
USING ranked r
WHERE m.ctid = r.ctid
  AND r.rn > 1;

CREATE UNIQUE INDEX IF NOT EXISTS uq_members_server_user
    ON members(server_id, user_id);
