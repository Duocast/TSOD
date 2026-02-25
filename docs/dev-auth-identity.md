# Dev auth identity notes

## Root cause summary

When two clients authenticate with the same legacy token (`dev`), the gateway issues the same auth identity (`user_id`) to both sessions.

- Presence/members are keyed by authenticated `user_id` per channel, so duplicate joins from two sessions collapse into one member row.
- Chat grouping keys by `author_user_id`, so messages from both sessions group as the same author when `author_user_id` is identical.
- Nickname/display name can differ between sessions, but that does not change auth identity.

## Operator note

- **Nickname != auth identity**
- To simulate different users in dev auth, use distinct tokens on each client, for example:
  - `VP_DEV_TOKEN=dev:overdose`
  - `VP_DEV_TOKEN=dev:dresk`
