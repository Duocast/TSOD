-- Seed baseline RBAC defaults for existing deployments.
INSERT INTO roles (id, server_id, name)
VALUES
  ('owner', '00000000-0000-0000-0000-000000000000', 'Owner'),
  ('admin', '00000000-0000-0000-0000-000000000000', 'Admin'),
  ('mod', '00000000-0000-0000-0000-000000000000', 'Moderator'),
  ('member', '00000000-0000-0000-0000-000000000000', '@everyone'),
  ('muted', '00000000-0000-0000-0000-000000000000', 'Muted')
ON CONFLICT (id) DO NOTHING;

INSERT INTO role_caps (role_id, cap, effect)
VALUES
  ('member', 'join_channel', 'grant'),
  ('member', 'speak', 'grant'),
  ('member', 'send_message', 'grant')
ON CONFLICT (role_id, cap, effect) DO NOTHING;
