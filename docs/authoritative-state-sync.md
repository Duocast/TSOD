# Authoritative state sync (first pass)

This document defines the initial authoritative snapshot architecture for client state.

## Connect/auth/sync flow

1. Client performs `Hello` + `AuthRequest`.
2. After auth, client enters `ConnectionStage::Syncing`.
3. Client requests `GetInitialStateSnapshotRequest`.
4. Server responds with `InitialStateSnapshot` and client applies it as the baseline before entering `Connected`.

## Snapshot contents

`InitialStateSnapshot` includes:

- `server_id`, `server_name`
- self identity (`self_user_id`, `self_display_name`)
- full channel list (`channels`)
- channel-scoped member snapshots (`channel_members`)
- `default_channel_id`
- `snapshot_version`

## Members panel semantics (v1)

Members are **selected-channel scoped**.

- Snapshot member payload is grouped by `channel_id`.
- Presence pushes (`member_joined`, `member_left`) apply to channel member lists.
- Server-wide connected-users semantics are not used for the members panel in this pass.

## Incremental updates and ordering

- Existing push events are still used after snapshot baseline.
- `ServerToClient` now exposes optional `event_seq` metadata.
- Outbox push messages are tagged with a timestamp-backed `event_seq` value.
- If client receives pushes with missing sequence metadata (`event_seq == 0`), client logs a TODO hook indicating forced resync should be triggered on suspected gaps.

## Reconnect/resync

Reconnect path reuses the same startup flow:

- reconnect => auth => request snapshot => apply authoritative state => connected.

This avoids reliance on stale local channel/member state across reconnects.
