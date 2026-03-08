# TSOD User Profiles — Design Specification

**Version:** 1.0  
**Target:** Production implementation for development team  
**Scope:** Full user profile system — data model, server storage, client UI, and real-time sync

---

## 1. Executive Summary

This document specifies the design for a production-grade user profile system in TSOD, a Rust-based self-hosted VoIP platform. The design draws from Discord's simplicity and Steam's visual personality, targeting gamers who want expressive profiles without overwhelming complexity.

### Current State

The codebase already has skeletal profile infrastructure that is mostly unimplemented:

- **Proto layer:** `user.proto` defines `UserProfile` with `display_name`, `avatar_asset_url`, `description`, `status`, `custom_status_text`, `badges`, `created_at`, `last_seen_at`. Control messages (`GetUserProfileRequest/Response`, `UpdateUserProfileRequest/Response`, `SetAvatarRequest/Response`) and push events (`UserStatusChanged`, `UserProfileUpdated`) are wired into `control.proto` at field numbers 70–77.
- **Client UI model:** `UserProfileData` struct exists with `user_id`, `display_name`, `description`, `status`, `badges` — but `UiEvent::UserProfileLoaded` is a no-op. `MemberEntry` carries `display_name`, `away_message`, and `avatar_url`.
- **User panel:** Bottom-left panel (`user_panel.rs`) renders a placeholder avatar circle with the user's initial letter, nick, online/offline text, and mute/deafen/share buttons. Clicking the avatar sets `show_set_avatar_dialog = true` but the dialog is minimal.
- **Members panel:** Right sidebar renders member rows with initial-letter avatars, voice state icons, and a context menu (local audio controls, poke, roles, kick). No profile card popup exists.
- **Server:** PostgreSQL (sqlx) with no `user_profiles` table. Auth is device-based Ed25519 or OIDC. Upload pipeline (`upload.proto`) exists with pre-signed URL flow.
- **Settings:** `AppSettings` persists `identity_nickname` locally but no profile editing UI beyond that.

### Design Philosophy

The profile system should feel like a natural extension of the existing architecture. TSOD's identity is device-based by default (no account server required for self-hosting), so profiles must work in both anonymous/device-auth and OIDC-authenticated deployments. Every feature should be achievable within egui's immediate-mode rendering model without requiring a web view.

---

## 2. Data Model

### 2.1 Extended Proto Definition — `user.proto`

The existing `UserProfile` message needs expansion. New fields should be added at the end to maintain wire compatibility.

```protobuf
message UserProfile {
  UserId user_id = 1;
  string display_name = 2;
  string avatar_asset_url = 3;
  AssetId avatar_asset_id = 4;
  string description = 5;             // "About Me" — supports markdown
  OnlineStatus status = 6;
  string custom_status_text = 7;
  repeated Badge badges = 8;
  Timestamp created_at = 9;
  Timestamp last_seen_at = 10;

  // ─── New fields ───
  string banner_asset_url = 11;       // profile banner image URL
  AssetId banner_asset_id = 12;
  uint32 accent_color = 13;           // ARGB packed, user-chosen highlight color
  string pronouns = 14;               // freeform, e.g. "he/him", "they/them"
  repeated ProfileLink links = 15;    // social/game links
  string custom_status_emoji = 16;    // emoji for custom status (unicode or asset ref)
  Timestamp custom_status_expires = 17; // optional auto-clear time
  GameActivity current_activity = 18; // "Playing Rust" rich presence
  AudioProfile audio_profile = 19;    // public-facing audio config (opt-in)
}

message ProfileLink {
  string platform = 1;   // "steam", "github", "twitter", "twitch", "youtube", "website"
  string url = 2;
  string display_text = 3; // optional custom label
  bool verified = 4;       // server-side verification flag (future)
}

message GameActivity {
  string game_name = 1;
  string details = 2;      // e.g. "Ranked — Gold II"
  string state = 3;        // e.g. "In Queue"
  Timestamp started_at = 4;
  string large_image_url = 5;  // game icon
}

message AudioProfile {
  string codec_name = 1;      // e.g. "Opus 510 kbps Music"
  uint32 bitrate_bps = 2;
  string quality_tier = 3;    // "Audiophile", "Music", "Voice"
}

message SetBannerRequest {
  AssetId asset_id = 1;
}

message SetBannerResponse {
  string banner_asset_url = 1;
}
```

### 2.2 Updated `UpdateUserProfileRequest`

The current proto uses raw fields which makes partial updates ambiguous. Recommended approach — add a field mask or use wrapper types for optional fields:

```protobuf
message UpdateUserProfileRequest {
  // Only set the fields you want to change.
  // Empty string = "clear this field"; absent = "don't touch".
  optional string display_name = 1;
  optional string description = 2;
  OnlineStatus status = 3;
  optional string custom_status_text = 4;
  optional string custom_status_emoji = 5;
  optional Timestamp custom_status_expires = 6;
  optional uint32 accent_color = 7;
  optional string pronouns = 8;
  repeated ProfileLink links = 9;     // full replacement when present
}
```

Using proto3 `optional` keyword generates `has_*` accessors in prost, enabling the server to distinguish "field not sent" from "field sent as empty/default."

### 2.3 PostgreSQL Schema — Migration `0022_user_profiles.sql`

```sql
CREATE TABLE IF NOT EXISTS user_profiles (
  user_id         UUID PRIMARY KEY,
  server_id       UUID NOT NULL,
  display_name    TEXT NOT NULL DEFAULT '',
  description     TEXT NOT NULL DEFAULT '',
  pronouns        TEXT NOT NULL DEFAULT '',
  accent_color    INTEGER NOT NULL DEFAULT 0,
  avatar_asset_id UUID NULL,
  banner_asset_id UUID NULL,
  links           JSONB NOT NULL DEFAULT '[]',
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_user_profiles_server ON user_profiles(server_id);

-- Badges are server-managed, not user-editable
CREATE TABLE IF NOT EXISTS user_badges (
  user_id    UUID NOT NULL,
  badge_id   TEXT NOT NULL,
  server_id  UUID NOT NULL,
  granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (user_id, badge_id)
);

CREATE TABLE IF NOT EXISTS badge_definitions (
  id          TEXT PRIMARY KEY,
  server_id   UUID NOT NULL,
  label       TEXT NOT NULL,
  icon_url    TEXT NOT NULL DEFAULT '',
  tooltip     TEXT NOT NULL DEFAULT '',
  position    INTEGER NOT NULL DEFAULT 0,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**Design decisions:**

- `links` is JSONB rather than a separate table because the data is always read and written as a unit, the cardinality is bounded (cap at 5–8 links), and JSONB avoids N+1 queries.
- `accent_color` is stored as a packed integer (ARGB) matching the proto representation, avoiding string parsing.
- `avatar_asset_id` and `banner_asset_id` reference the existing upload pipeline's asset system. The gateway resolves these to URLs when building the proto response.
- Badges are a separate table because they're server-managed (awarded by admins/automation) and need independent query patterns (e.g., "list all users with badge X").

### 2.4 Client-Side Model — `UiModel` Updates

```rust
/// Full profile data for popup/card rendering.
#[derive(Debug, Clone, Default)]
pub struct UserProfileData {
    pub user_id: String,
    pub display_name: String,
    pub description: String,           // markdown
    pub pronouns: String,
    pub status: OnlineStatus,
    pub custom_status_text: String,
    pub custom_status_emoji: String,
    pub accent_color: u32,             // ARGB
    pub avatar_url: Option<String>,
    pub banner_url: Option<String>,
    pub badges: Vec<BadgeData>,
    pub links: Vec<ProfileLinkData>,
    pub created_at: i64,               // unix millis
    pub last_seen_at: i64,
    pub current_activity: Option<GameActivityData>,
    pub roles: Vec<RoleData>,          // resolved from permissions system
}

#[derive(Debug, Clone)]
pub struct BadgeData {
    pub id: String,
    pub label: String,
    pub icon_url: String,
    pub tooltip: String,
}

#[derive(Debug, Clone)]
pub struct ProfileLinkData {
    pub platform: String,
    pub url: String,
    pub display_text: String,
    pub verified: bool,
}

#[derive(Debug, Clone)]
pub struct GameActivityData {
    pub game_name: String,
    pub details: String,
    pub state: String,
    pub started_at: i64,
    pub large_image_url: String,
}

#[derive(Debug, Clone)]
pub struct RoleData {
    pub name: String,
    pub color: u32,
    pub position: i32,
}
```

Add to `UiModel`:

```rust
// Profile popup state
pub profile_popup_user_id: Option<String>,
pub profile_popup_data: Option<UserProfileData>,
pub profile_popup_loading: bool,
pub profile_popup_anchor: Option<egui::Pos2>,  // screen position for popup placement

// Profile editing state
pub show_edit_profile: bool,
pub edit_profile_tab: ProfileEditTab,
pub edit_profile_draft: UserProfileEditDraft,

// Profile cache (avoids re-fetching on every hover)
pub profile_cache: HashMap<String, CachedProfile>,
```

```rust
#[derive(Debug, Clone, Default)]
pub struct UserProfileEditDraft {
    pub display_name: String,
    pub description: String,
    pub pronouns: String,
    pub accent_color: u32,
    pub custom_status_text: String,
    pub custom_status_emoji: String,
    pub links: Vec<ProfileLinkData>,
}

#[derive(Debug, Clone)]
pub struct CachedProfile {
    pub data: UserProfileData,
    pub fetched_at: std::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileEditTab {
    Profile,
    Links,
    Avatar,
    Banner,
}
```

---

## 3. Profile Popup (User Card)

This is the primary way users interact with other users' profiles. It appears when clicking a username in the members list, chat messages, or DM list.

### 3.1 Layout Specification

The popup is an `egui::Window` rendered as a floating card (not a system window). Target dimensions: **340×420 px** at default UI scale.

```
┌──────────────────────────────────┐
│  ░░░░░░ BANNER IMAGE ░░░░░░░░░   │  ← 340 × 100 px, or accent_color gradient
│  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░   │
│ ┌────┐                           │
│ │    │ DisplayName               │  ← Avatar overlaps banner bottom edge
│ │ AV │ ● Online                  │    64×64 circle, 3px border in accent_color
│ │    │                           │
│ └────┘                           │
│──────────────────────────────────│
│ 🎮 Playing Rust — 2h 14m        │  ← Activity row (conditional)
│──────────────────────────────────│
│ ✨ Custom status text here       │  ← Custom status (conditional)
│──────────────────────────────────│
│ ABOUT ME                         │
│ Audiophile gamer, Rust dev.      │  ← Markdown-rendered description
│ Building cool things.            │    Max 190 chars displayed, "..." truncation
│──────────────────────────────────│
│ ROLES                            │
│ [Owner] [Admin] [Member]         │  ← Colored role pills
│──────────────────────────────────│
│ LINKS                            │
│ 🎮 Steam  ·  🐙 GitHub          │  ← Platform icons + labels
│──────────────────────────────────│
│ 🏷️ Early Adopter  🎖️ Staff      │  ← Badge row
│──────────────────────────────────│
│ Member since Jan 15, 2025        │  ← Footer
│  [Message]  [Poke]  [···]        │  ← Action buttons
└──────────────────────────────────┘
```

### 3.2 Rendering Strategy in egui

**Banner:** Use `egui::Image::from_uri()` for the banner URL. If no banner is set, render a gradient using the user's `accent_color` (lerp from accent to `bg_dark()` over the banner height). Clip to rounded top corners via `ui.painter().rect_filled()` as a mask layer.

**Avatar overlay:** The avatar should visually overlap the banner's bottom edge by ~20px. In egui this means:
1. Reserve the banner space (100px tall).
2. Use `ui.put()` or manual `ui.painter()` calls to position the avatar circle at an absolute position (left-aligned, vertically spanning the banner/body boundary).
3. Draw the avatar circle with a 3px ring in `accent_color`, then the avatar image clipped to a circle.

**Accent color integration:** The `accent_color` should tint the banner fallback gradient, the avatar border ring, and the "Message" button fill. This gives each profile a personalized feel (similar to Discord's profile accent theming).

**Activity row:** Only shown when `current_activity` is `Some`. Render with a game controller emoji, the game name in bold, and an elapsed timer calculated from `started_at`. Use `ui.ctx().request_repaint_after(Duration::from_secs(60))` to update the timer without continuous repainting.

**Role pills:** Render as inline colored rectangles with rounded corners. Each pill's background is the role color at 20% opacity, with text in the role color. Sort by `position` descending (highest role first).

**Badge row:** Render badge icons as small (16×16) images from `icon_url`, with a tooltip showing the badge `tooltip` text on hover. If icons aren't loaded yet, show the badge `label` as fallback text.

**Action buttons:** "Message" opens/creates a DM channel (uses existing `CreateDmChannelRequest`). "Poke" opens the existing poke dialog pre-filled. The "···" overflow menu contains: Copy User ID, Roles, Connection Info, Mute for me, Volume slider, Kick, Ban.

### 3.3 Popup Lifecycle

1. **Trigger:** User clicks a display name in the members panel, chat author name, or DM list. Set `profile_popup_user_id = Some(user_id)`, `profile_popup_anchor = Some(click_pos)`, `profile_popup_loading = true`.
2. **Fetch:** Send `GetUserProfileRequest` via the control stream. Check `profile_cache` first — if cache entry exists and is < 60 seconds old, use it immediately and skip the network request.
3. **Render:** On `GetUserProfileResponse`, populate `profile_popup_data`, set `profile_popup_loading = false`, insert into `profile_cache`.
4. **Dismiss:** Click outside the popup, press Escape, or click the X button. Set `profile_popup_user_id = None`.

### 3.4 Profile Cache Design

```rust
const PROFILE_CACHE_TTL: Duration = Duration::from_secs(60);
const PROFILE_CACHE_MAX_ENTRIES: usize = 100;

impl UiModel {
    pub fn get_cached_profile(&self, user_id: &str) -> Option<&UserProfileData> {
        self.profile_cache.get(user_id).and_then(|cached| {
            if cached.fetched_at.elapsed() < PROFILE_CACHE_TTL {
                Some(&cached.data)
            } else {
                None
            }
        })
    }

    pub fn insert_profile_cache(&mut self, user_id: String, data: UserProfileData) {
        if self.profile_cache.len() >= PROFILE_CACHE_MAX_ENTRIES {
            // Evict oldest entry
            if let Some(oldest_key) = self.profile_cache.iter()
                .min_by_key(|(_, v)| v.fetched_at)
                .map(|(k, _)| k.clone())
            {
                self.profile_cache.remove(&oldest_key);
            }
        }
        self.profile_cache.insert(user_id, CachedProfile {
            data,
            fetched_at: std::time::Instant::now(),
        });
    }
}
```

Invalidation: When a `UserProfileUpdated` push event arrives, remove the corresponding cache entry so the next popup fetch gets fresh data.

---

## 4. Self-Profile Editing

### 4.1 Profile Edit Modal

Accessible from: clicking own avatar in the user panel (bottom-left), or a new "Edit Profile" button in the Settings > Identity section.

The edit modal is an `egui::Window` with tabs:

**Profile Tab:**
- Display Name (text input, max 32 chars, validated for no-whitespace-only)
- Pronouns (text input, max 24 chars)
- About Me (multiline text area, max 190 chars, with char counter)
- Accent Color (color picker — use a grid of preset colors plus a hex input)

**Links Tab:**
- List of current links with remove buttons
- "Add Link" button → dropdown for platform + URL input
- Supported platforms: Steam, GitHub, Twitter/X, Twitch, YouTube, Website (custom)
- Max 5 links
- URL validation on input

**Avatar Tab:**
- Current avatar preview (large, 128×128)
- "Upload" button → opens native file picker (`rfd::FileDialog`) for PNG/JPG/WEBP, max 8 MB
- "Remove" button to clear avatar back to initial-letter default
- Upload flow: uses existing `CreateUploadRequest` → HTTP PUT → `CompleteUploadRequest` → `SetAvatarRequest`
- Client-side image resize before upload: scale down to 256×256 max using the `image` crate (already a dependency), convert to WebP for optimal size

**Banner Tab:**
- Current banner preview (full-width, scaled)
- "Upload" button → same file picker, PNG/JPG/WEBP, max 10 MB
- "Remove" button
- Recommended dimensions shown as hint text: "Recommended: 680×240 px"
- Client-side resize to 680×240 max, WebP conversion
- Upload via same pipeline, with new `SetBannerRequest`

**Save/Cancel:** Bottom bar with "Save Changes" (sends `UpdateUserProfileRequest`) and "Cancel" (resets draft to current profile).

### 4.2 Custom Status

A dedicated quick-action accessible from the user panel (not the full edit modal). Clicking the status area in the user panel opens a small popover:

```
┌─────────────────────────────┐
│ Set Custom Status            │
│                              │
│ 😊 ▾  [Status text here   ] │
│                              │
│ Clear after: [Don't clear ▾]│
│   · 30 minutes               │
│   · 1 hour                   │
│   · 4 hours                  │
│   · Today                    │
│   · Don't clear              │
│                              │
│ ── Set Status ──             │
│                              │
│ ● Online                     │
│ ○ Idle                       │
│ ○ Do Not Disturb             │
│ ○ Invisible                  │
│                              │
│     [Save]  [Clear Status]   │
└─────────────────────────────┘
```

The emoji selector can start as a simple text input (type a unicode emoji) and later be upgraded to a picker grid.

### 4.3 Avatar Upload Pipeline

This uses the existing upload infrastructure with a profile-specific wrapper:

```
User selects file
    → Client: validate file type (PNG/JPG/WEBP/GIF), max 8 MB
    → Client: decode & resize to 256×256 using `image` crate
    → Client: encode to WebP via `image` crate's WebP encoder
    → Client: send CreateUploadRequest (mime_type: "image/webp", size_bytes: N)
    → Server: returns upload_url (pre-signed) + asset_id
    → Client: HTTP PUT to upload_url with WebP bytes
    → Client: send CompleteUploadRequest(asset_id)
    → Server: returns download_url
    → Client: send SetAvatarRequest(asset_id)
    → Server: updates user_profiles.avatar_asset_id, resolves URL, returns SetAvatarResponse
    → Server: broadcasts UserProfileUpdated event to all connected users
    → Client: updates local profile, refreshes user panel avatar
```

The same flow applies to banners with `SetBannerRequest` (new message in control.proto at field number 73, response at 78).

---

## 5. User Panel Redesign (Bottom-Left)

The current user panel is functional but needs to integrate profile features.

### 5.1 Updated Layout

```
┌──────────────────────────────────┐
│ ┌────┐                     ⚙️    │
│ │    │ DisplayName                │
│ │ AV │ ✨ Custom status text      │ ← click name/status area → edit status popover
│ │    │ 🎮 Playing Rust            │ ← activity (conditional)
│ └────┘                           │
│                                  │
│ [🌙] [🎤] [🔊] [🖥️] [⚙️]      │ ← away, mute, deafen, share, settings
│ ▓▓▓▓▓▓▓░░░░░░░░ VAD             │ ← voice activity bar
└──────────────────────────────────┘
```

### 5.2 Behavioral Changes

- **Avatar click:** Opens profile edit modal (not just the avatar upload dialog).
- **Name/status area click:** Opens custom status popover for quick status changes.
- **Right-click avatar:** Context menu with "Edit Profile", "Set Status", "Copy User ID".
- **Avatar image:** Load from `avatar_url` via `egui::Image::from_uri()`. Display with a circular clip and status indicator dot (existing behavior, refined with accent_color ring).
- **Status indicator colors:** Map from `OnlineStatus` enum — Online (green), Idle (yellow), DND (red), Invisible/Offline (gray). These already exist in `theme.rs` as `COLOR_ONLINE`, `COLOR_IDLE`, `COLOR_DND`, `COLOR_OFFLINE`.

---

## 6. Members Panel Profile Integration

### 6.1 Member Row Enhancement

Each member row in the right sidebar should be updated:

```
┌──────────────────────────────────┐
│ ┌──┐ DisplayName     ▓▓▓░░ VU   │
│ │AV│ 🎮 Playing Rust             │  ← activity or status text, truncated
│ └──┘                             │
└──────────────────────────────────┘
```

- **Left-click** member row: Opens profile popup (section 3).
- **Right-click** member row: Opens context menu (existing behavior, plus "View Profile" at top).
- **Middle-click** member row: Opens connection info (existing behavior).
- Avatar now loads from `avatar_url` if available, with circular clip.
- If the member has a non-default `accent_color`, use it as a subtle left-border accent on hover.

### 6.2 Chat Author Profile Integration

When a user clicks on a message author's name in the chat panel:
- Open the profile popup anchored near the author name.
- The author name in chat should render in the user's highest role color (fetched from the permissions/roles system already in place).

---

## 7. Server-Side Implementation

### 7.1 Gateway Handler Additions

The gateway (`gateway.rs`) needs handlers for the profile-related control messages. These should follow the existing pattern of matching on `ClientToServer::payload` variants:

**GetUserProfile handler:**
1. Validate auth (session must be authenticated).
2. Query `user_profiles` table by `user_id`. If no row exists, return a default profile with just the display name from `channel_members` or auth.
3. Query `user_badges` joined with `badge_definitions` for this user.
4. Query `user_roles` joined with `roles` for role display data.
5. Resolve `avatar_asset_id` and `banner_asset_id` to URLs via the asset storage system.
6. Build and return `GetUserProfileResponse`.

**UpdateUserProfile handler:**
1. Validate auth.
2. Validate field lengths (display_name ≤ 32, description ≤ 190, pronouns ≤ 24, links ≤ 5 items, link URLs ≤ 256 chars).
3. Upsert into `user_profiles` using `ON CONFLICT (user_id) DO UPDATE`.
4. Broadcast `UserProfileUpdated` event via `PushHub` to all connected users on this server.
5. Return `UpdateUserProfileResponse` with the full updated profile.

**SetAvatar / SetBanner handlers:**
1. Validate auth.
2. Verify the `asset_id` exists and belongs to this user (query the upload system).
3. Update `user_profiles.avatar_asset_id` or `banner_asset_id`.
4. Resolve new URL.
5. Broadcast `UserProfileUpdated`.
6. Return response with new URL.

### 7.2 Profile Auto-Creation

On first authentication (in the auth handler, after `ensure_baseline_role_assignment`), insert a default profile row:

```sql
INSERT INTO user_profiles (user_id, server_id, display_name)
VALUES ($1, $2, $3)
ON CONFLICT (user_id) DO NOTHING;
```

The display name defaults to the `preferred_display_name` from `AuthRequest`, or the device ID's first 8 chars as a fallback.

### 7.3 InitialStateSnapshot Extension

The `InitialStateSnapshot` message should include the self-user's profile to avoid a separate round-trip on connect:

```protobuf
message InitialStateSnapshot {
  // ... existing fields ...
  UserProfile self_profile = 9;   // full profile for the connecting user
}
```

This lets the client immediately render the user panel with real profile data instead of showing placeholder state.

### 7.4 Profile in PresenceEvent / ChannelMember

When a user joins a channel, the `ChannelMember` message in `MemberJoined` should include the avatar URL so other clients can render it without a separate profile fetch. Add to `ChannelMember`:

```protobuf
message ChannelMember {
  // ... existing fields ...
  string avatar_asset_url = 10;
  string away_message = 11;
  uint32 accent_color = 12;
}
```

This is a denormalization for performance — the full profile is fetched on-demand when the popup is opened, but the avatar and accent color are available immediately for member list rendering.

---

## 8. Rich Presence / Game Activity

### 8.1 Design Approach

Game activity detection should be handled entirely client-side. The client periodically scans running processes and matches against a known game database. This avoids server complexity and keeps the feature working in self-hosted deployments without an external API.

**Client-side detection flow:**
1. On a 30-second timer, enumerate running processes.
2. Match executable names against a bundled game database (a JSON file shipped with the client, similar to Discord's game detection list).
3. If a match is found and differs from the current activity, send an `UpdateUserProfileRequest` with the `current_activity` field.
4. If the game process exits, send an update clearing the activity.

**Game database format:**

```json
{
  "games": [
    {
      "exe_names": ["RustClient.exe", "rust"],
      "display_name": "Rust",
      "icon_asset": "rust_icon"
    }
  ]
}
```

**Process enumeration:**
- Windows: Use `windows::Win32::System::ProcessStatus::EnumProcesses` + `GetModuleBaseNameW` (already using the `windows` crate).
- Linux: Read `/proc/*/comm` or `/proc/*/exe` symlinks.

**Privacy controls in settings:**
- "Share game activity" toggle (default: on).
- "Share game details" toggle (default: on) — when off, only "Playing a game" is shown without the game name.

### 8.2 Proto Integration

The `GameActivity` message (defined in section 2.1) is set via `UpdateUserProfileRequest`. The server stores it transiently in memory (not in the database) since it's session-scoped. When the user disconnects, the server clears the activity and broadcasts a `UserProfileUpdated` event.

---

## 9. Badge System

### 9.1 Badge Types

Badges are server-managed awards that appear on user profiles. They're a lightweight recognition system.

**Built-in badges** (auto-assigned by the server):
- **Early Adopter** — assigned to users who created profiles before a configurable date threshold.
- **Server Owner** — assigned to users with the "owner" role.
- **Server Admin** — assigned to users with the "admin" role.
- **Verified** — future: linked and verified external account.

**Custom badges** (admin-created):
- Server admins can create badges with custom names, icons, and tooltips via a new admin panel section.
- Admins can grant/revoke custom badges per user.

### 9.2 Badge Icon Assets

Badge icons are 64×64 PNG/WebP images uploaded through the existing upload pipeline. The `badge_definitions.icon_url` stores the resolved download URL. The client renders them at 16×16 in the profile popup and 20×20 in a dedicated "badges" section if the user has many.

### 9.3 Admin API

New control messages:

```protobuf
message CreateBadgeRequest {
  string id = 1;           // slug, e.g. "early-adopter"
  string label = 2;
  AssetId icon_asset_id = 3;
  string tooltip = 4;
}

message GrantBadgeRequest {
  UserId user_id = 1;
  string badge_id = 2;
}

message RevokeBadgeRequest {
  UserId user_id = 1;
  string badge_id = 2;
}
```

These should be wired into `control.proto` at new field numbers in the admin range (e.g., 215–220) and gated by the `manage_badges` capability (new cap to add in the permissions system).

---

## 10. Technology Decisions

### 10.1 Image Handling

**Client-side processing:**
- Use the `image` crate (already a dependency) for decode, resize, and format conversion.
- Avatars: resize to 256×256, encode as WebP.
- Banners: resize to 680×240, encode as WebP.
- WebP provides superior compression at equivalent quality, reducing upload times and storage.

**Server-side processing:**
- The server already generates thumbnails in `CompleteUploadResponse`. For profile images, thumbnails aren't needed (the client-side resize handles this). The server should validate that the uploaded file is a valid image and within size limits.

**Client-side caching (avatar/banner textures):**
- egui's built-in `Image::from_uri()` handles HTTP fetching and texture caching. For self-hosted servers using HTTP (not HTTPS), ensure the egui loader is configured to allow HTTP URIs.
- Consider using `egui_extras::RetainedImage` or a manual texture cache if URI-based loading proves insufficient for the number of avatars in a 50-user channel.

### 10.2 Markdown Rendering in "About Me"

The "About Me" field supports a **limited subset** of Markdown for safety and performance:

- Bold (`**text**`), italic (`*text*`), inline code (`` `code` ``), links (`[text](url)`), and emoji shortcodes (`:smile:`).
- No headings, images, tables, or HTML.
- Rendering is done client-side using a lightweight custom parser (not a full Markdown library) that outputs styled `RichText` runs for egui.

This keeps the rendering fast (no layout engine overhead), prevents abuse (no embedded images or oversized headings), and is simple to implement.

```rust
pub fn render_about_me(ui: &mut egui::Ui, text: &str) {
    // Parse inline markdown to a Vec<StyledSpan>
    let spans = parse_inline_markdown(text);
    let mut job = egui::text::LayoutJob::default();
    for span in spans {
        match span {
            StyledSpan::Normal(t) => {
                job.append(&t, 0.0, egui::TextFormat {
                    font_id: egui::FontId::proportional(13.0),
                    color: theme::text_color(),
                    ..Default::default()
                });
            }
            StyledSpan::Bold(t) => {
                job.append(&t, 0.0, egui::TextFormat {
                    font_id: egui::FontId::new(13.0, egui::FontFamily::Proportional),
                    color: theme::text_color(),
                    // Bold via font weight — egui supports this in recent versions
                    ..Default::default()
                });
            }
            StyledSpan::Code(t) => {
                job.append(&t, 0.0, egui::TextFormat {
                    font_id: egui::FontId::monospace(12.0),
                    color: theme::text_dim(),
                    background: theme::bg_input(),
                    ..Default::default()
                });
            }
            StyledSpan::Link { text: t, url } => {
                // Render as colored text; store URL for click handling
                job.append(&t, 0.0, egui::TextFormat {
                    font_id: egui::FontId::proportional(13.0),
                    color: theme::COLOR_LINK,
                    underline: egui::Stroke::new(1.0, theme::COLOR_LINK),
                    ..Default::default()
                });
            }
        }
    }
    ui.label(job);
}
```

### 10.3 Color Picker for Accent Color

Use a grid of 18 preset colors (similar to Discord's role color picker) plus a hex input for custom colors:

```
Preset grid (3 rows × 6 columns):
  #5865F2 (blurple)  #EB459E (fuchsia)  #ED4245 (red)
  #FEE75C (yellow)   #57F287 (green)    #3BA55C (dark green)
  #5BC0EB (sky blue) #9B59B6 (purple)   #E67E22 (orange)
  #1ABC9C (teal)     #E91E63 (pink)     #607D8B (slate)
  #2C2F33 (dark)     #99AAB5 (light)    #FFFFFF (white)
  #000000 (black)    #34495E (charcoal) #71368A (violet)

Hex input: [#5865F2  ] [Preview ●]
```

Implementation: render colored squares as clickable buttons using `ui.painter().rect_filled()`, with a selection ring around the active color. The hex input is a standard `egui::TextEdit` with validation.

---

## 11. Performance Considerations

### 11.1 Profile Fetch Batching

When 50 users are in a voice channel and the client renders the member list, it should NOT fire 50 individual `GetUserProfileRequest` calls. Instead:

- Member list rendering only needs `display_name`, `avatar_url`, and `accent_color` — all available from the `ChannelMember` message (after the proto extension in section 7.4).
- Full profile fetches happen only on demand (popup click).
- The 60-second profile cache prevents redundant fetches on repeated clicks.

### 11.2 Avatar Texture Management

With 50 members, the client may need 50 avatar textures loaded simultaneously. Strategy:

- Use `egui::Image::from_uri()` which handles lazy loading and LRU eviction internally.
- For members not currently visible in the scroll area, their textures will naturally be evicted.
- Avatar images are small (256×256 WebP, typically 10–30 KB), so 50 textures is ~1.5 MB of VRAM — negligible.

### 11.3 Real-Time Updates

- `UserProfileUpdated` events are broadcast to all connected users when any profile changes. For a 50-user server, this means 49 push messages per profile update — trivially handled by the existing `PushHub`.
- `UserStatusChanged` events are already lightweight (just user_id + status enum + text).
- Game activity updates are throttled to 30-second intervals on the client side, preventing excessive update chatter.

---

## 12. Security and Validation

### 12.1 Input Validation (Server-Side)

All profile fields must be validated on the server before persistence:

| Field | Max Length | Additional Constraints |
|-------|-----------|----------------------|
| display_name | 32 chars | Must have at least 1 non-whitespace char. Strip leading/trailing whitespace. No control characters. |
| description | 190 chars | Strip control characters except newlines. |
| pronouns | 24 chars | Strip control characters. |
| custom_status_text | 64 chars | Strip control characters. |
| links[].url | 256 chars | Must be valid URL with `https://` or `http://` scheme. Max 5 links. |
| links[].display_text | 32 chars | Strip control characters. |
| accent_color | 4 bytes | Must be valid ARGB (alpha byte ignored in rendering, set to 0xFF). |

### 12.2 Rate Limiting

Profile updates should be rate-limited to prevent abuse:
- `UpdateUserProfileRequest`: 5 requests per minute per user.
- `SetAvatarRequest` / `SetBannerRequest`: 3 requests per 10 minutes per user.
- `GetUserProfileRequest`: 30 requests per minute per user.

Implement using a per-user token bucket in the gateway (can use `dashmap` with a timestamp + counter, similar to the existing session tracking).

### 12.3 Content Moderation Hooks

The profile system should emit audit log entries for profile changes, enabling server admins to review modifications:

```sql
INSERT INTO audit_log (id, server_id, actor_user_id, action, target_type, target_id, context)
VALUES ($1, $2, $3, 'user.profile_update', 'user', $4, $5);
```

The `context` JSONB should contain a diff of changed fields (not the full profile) for efficient review.

### 12.4 Avatar/Banner Content Safety

Server-side, after upload completion:
1. Verify the file is a valid image (decode it with the `image` crate — already a dependency).
2. Verify dimensions are within expected bounds (avatars ≤ 512×512, banners ≤ 1920×480).
3. Reject animated GIFs over 2 MB (if GIF support is added later).

Client-side pre-upload validation prevents most invalid uploads, but server validation is the authoritative check.

---

## 13. Migration and Rollout Plan

### Phase 1 — Foundation (Week 1–2)
1. Add PostgreSQL migration `0022_user_profiles.sql`.
2. Implement server-side `GetUserProfile`, `UpdateUserProfile`, `SetAvatar` handlers.
3. Auto-create profile rows on first auth.
4. Extend `InitialStateSnapshot` with `self_profile`.
5. Wire up the client dispatcher to handle profile responses and push events.

### Phase 2 — Self Profile Editing (Week 2–3)
1. Build the profile edit modal (Profile tab only: display name, description, pronouns).
2. Implement accent color picker.
3. Implement avatar upload flow (resize + WebP + upload pipeline + SetAvatar).
4. Implement banner upload flow.
5. Build the custom status popover.
6. Update the user panel to render real profile data.

### Phase 3 — Profile Popup (Week 3–4)
1. Build the profile popup window with banner, avatar overlay, role pills, and action buttons.
2. Implement profile cache with TTL and invalidation.
3. Hook up click handlers in members panel and chat panel.
4. Implement inline markdown renderer for About Me.

### Phase 4 — Social Features (Week 4–5)
1. Implement profile links (add/edit/remove in edit modal, render in popup).
2. Implement the badge system (schema, admin API, rendering).
3. Extend `ChannelMember` with avatar/accent denormalization.
4. Game activity detection (process scanning, game database, privacy settings).

### Phase 5 — Polish (Week 5–6)
1. Animate popup open/close (egui `animate_bool` for opacity fade).
2. Add role-colored display names in chat.
3. Profile preview in edit modal ("This is how others will see your profile").
4. Accessibility: tooltip text for all badge/status icons, keyboard navigation for popup.
5. Performance testing with 50 concurrent users.

---

## 14. Recommended Crate Additions

| Crate | Purpose | Notes |
|-------|---------|-------|
| (none for client) | — | All needed crates are already dependencies (`image`, `egui`, `rfd`, `serde_json`, etc.) |
| (none for server) | — | `sqlx`, `image`, `uuid`, `chrono` are already present |

The existing dependency set is sufficient. No new crate additions are recommended — this keeps the build lean and avoids dependency bloat. The `image` crate handles all resize/encode needs. `egui` provides all the rendering primitives for the popup and edit modal. `rfd` handles native file dialogs.

---

## 15. Summary of Design Decisions

1. **Profiles are per-server, not global.** Since TSOD is self-hosted, each server instance maintains its own profile data. If a user connects to multiple servers, they have independent profiles on each. This is the correct model for self-hosted software.

2. **Device-auth compatible.** Profiles work without OIDC. The device identity (Ed25519 keypair) provides the stable user_id. Users can personalize their profile on any server without creating an account.

3. **Denormalized avatar in ChannelMember.** The avatar URL is duplicated in the member list wire format to avoid 50 profile fetches on channel join. Full profiles are lazy-loaded on demand.

4. **Client-side image processing.** Resize and WebP conversion happen before upload. This reduces server CPU load, upload times, and storage costs. The server validates but doesn't re-process.

5. **60-second profile cache with push invalidation.** Balances freshness with performance. Push events ensure changes are visible promptly without polling.

6. **Limited markdown in About Me.** Inline formatting only (bold, italic, code, links). No block elements. This prevents abuse and keeps rendering simple in egui's immediate mode.

7. **Game activity is client-local.** Process scanning happens on the client. The server only stores the current activity transiently in memory. No external API dependency.

8. **Badges are server-managed.** Users cannot self-assign badges. This maintains their value as recognition markers and gives admins control.

9. **Accent color drives visual personalization.** A single user-chosen color tints the banner gradient, avatar ring, and action buttons. This is the highest-impact personalization feature for the lowest implementation cost.

10. **No WebView dependency.** Everything renders natively in egui. This preserves TSOD's lightweight, fast-startup character and avoids the complexity/size of embedding a browser engine.
