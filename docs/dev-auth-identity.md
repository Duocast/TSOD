# Device auth identity notes

Legacy shared `dev` tokens are no longer used for client auth.

Current behavior in dev mode:
- Each client install creates a local Ed25519 device keypair and a `device_id` UUID.
- During `HelloAck`, the gateway returns a per-connection `auth_challenge`.
- The client signs `(auth_challenge || session_id)` and sends `DeviceAuth`.
- The gateway verifies the signature with `device_pubkey` and maps the key to a stable `user_id`.

Identity model:
- `user_id` is stable for a registered device key.
- `display_name` is presentation only and never changes identity.
- Different device keys authenticate as different users by default.
