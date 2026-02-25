ALTER TABLE channels
  DROP CONSTRAINT IF EXISTS channels_parent_id_fkey;

ALTER TABLE channels
  ADD CONSTRAINT channels_parent_id_fkey
  FOREIGN KEY (parent_id)
  REFERENCES channels(id)
  ON DELETE CASCADE;
