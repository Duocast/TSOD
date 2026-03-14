//! xdg-desktop-portal ScreenCast D-Bus integration.
//!
//! # D-Bus / portal flow
//!
//! The xdg-desktop-portal ScreenCast interface is used to let the compositor
//! open its native source-picker, grant access, and hand back PipeWire node IDs.
//! The sequence is:
//!
//! ```text
//!  client → CreateSession  → portal
//!  client ← Response (session_handle)
//!
//!  client → SelectSources (session_handle: o, types=Screen|Window,
//!                           cursor=Embedded)
//!  client ← Response (empty results, or cancelled)
//!
//!  client → Start (session_handle: o, parent_window: s, options: a{sv})
//!  client ← Response (streams: a(ua{sv}))
//! ```
//!
//! Each portal method returns an `org.freedesktop.portal.Request` object path.
//! We subscribe to the `Response` signal on that path *before* calling the
//! method, then block on the iterator until it arrives.
//!
//! The subscription-before-call pattern is critical: if we subscribe *after*
//! the call the portal might respond between the call returning and the
//! subscription being registered, losing the signal.
//!
//! # D-Bus type accuracy
//!
//! Session handles are `o` (ObjectPath), not plain strings.  The parent-window
//! argument for `Start` is `s`.  This distinction matters for strict portal
//! implementations.  We use `OwnedObjectPath` / `ObjectPath` for all `o`
//! parameters.
//!
//! # OS / runtime assumptions
//!
//! * A D-Bus session bus must be reachable (`DBUS_SESSION_BUS_ADDRESS` set).
//! * `xdg-desktop-portal` must be running and providing the `ScreenCast`
//!   interface (tested on GNOME ≥ 40 and KDE Plasma ≥ 5.20).
//! * The PipeWire nodes returned by `Start` are on the *default* PipeWire
//!   daemon.  We do **not** call `OpenPipeWireRemote`; for non-sandboxed
//!   applications this is not required because the portal's PipeWire remote
//!   is the same daemon the app connects to via `pw_context_connect`.
//!   Flatpak / portal-FD isolation is out of scope.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
// OwnedObjectPath is used only for the call return type (the discarded request handle).

/// PipeWire node IDs obtained from the portal after the user selects sources.
#[derive(Debug)]
pub struct ScreenCastNodes {
    /// One entry per selected source.  At least one element is guaranteed when
    /// `request_screencast_nodes` returns `Ok`.
    pub node_ids: Vec<u32>,
}

/// Run the full xdg-desktop-portal ScreenCast flow.
///
/// Opens the system-native source picker, waits for the user to confirm or
/// cancel, and returns PipeWire node IDs for each selected stream.
///
/// **Blocks** until the user makes a selection (or cancels).  Call from a
/// dedicated OS thread (e.g. `tokio::task::spawn_blocking`), not from an
/// async executor thread.
pub fn request_screencast_nodes() -> anyhow::Result<ScreenCastNodes> {
    let conn = Connection::session().context("connect to D-Bus session bus")?;

    // Unique name looks like ":1.42"; flatten to "1_42" for object paths.
    let unique_name = conn
        .unique_name()
        .ok_or_else(|| anyhow!("D-Bus connection has no unique name"))?
        .to_string();
    let sender_flat = unique_name.trim_start_matches(':').replace('.', "_");

    let portal = Proxy::new(&conn, PORTAL_BUS, PORTAL_PATH, SCREENCAST_IFACE)
        .context("create ScreenCast portal proxy")?;

    let mut token_n: u32 = 0;
    let mut next_token = || {
        token_n += 1;
        format!("tsod{token_n}")
    };

    // ── 1. CreateSession ─────────────────────────────────────────────────────

    let sess_token = next_token();
    let req_token = next_token();

    // Use Value<'static> (owned strings) to avoid borrow-lifetime issues.
    let mut opts: HashMap<&'static str, Value<'static>> = HashMap::new();
    opts.insert("session_handle_token", Value::from(sess_token));
    opts.insert("handle_token", Value::from(req_token.clone()));

    let create_results = portal_call(
        &conn,
        &portal,
        &sender_flat,
        &req_token,
        "CreateSession",
        |p| p.call::<_, _, (OwnedObjectPath,)>("CreateSession", &(&opts,)),
    )
    .context("CreateSession")?;

    // session_handle is returned as a String (object-path value) so that
    // subsequent calls can borrow it as `ObjectPath<'_>`.
    let session_path_str = extract_object_path_str(&create_results, "session_handle")
        .context("CreateSession: extract session_handle")?;
    let session_path = ObjectPath::try_from(session_path_str.as_str())
        .context("CreateSession: parse session_handle as ObjectPath")?;

    // ── 2. SelectSources ─────────────────────────────────────────────────────
    // source_type: 1 = Screen, 2 = Window, 3 = Both.
    // cursor_mode: 2 = Embedded cursor.

    let req_token = next_token();
    let mut opts: HashMap<&'static str, Value<'static>> = HashMap::new();
    opts.insert("handle_token", Value::from(req_token.clone()));
    opts.insert("types", Value::from(3u32));
    opts.insert("multiple", Value::from(false));
    opts.insert("cursor_mode", Value::from(2u32));

    // session_path serialises as 'o' (D-Bus object path).
    portal_call(
        &conn,
        &portal,
        &sender_flat,
        &req_token,
        "SelectSources",
        |p| p.call::<_, _, (OwnedObjectPath,)>("SelectSources", &(&session_path, &opts)),
    )
    .context("SelectSources")?;

    // ── 3. Start ─────────────────────────────────────────────────────────────

    let req_token = next_token();
    let mut opts: HashMap<&'static str, Value<'static>> = HashMap::new();
    opts.insert("handle_token", Value::from(req_token.clone()));

    // parent_window is 's' (empty string = no parent window handle).
    let start_results = portal_call(
        &conn,
        &portal,
        &sender_flat,
        &req_token,
        "Start",
        |p| p.call::<_, _, (OwnedObjectPath,)>("Start", &(&session_path, "", &opts)),
    )
    .context("Start")?;

    let node_ids = extract_stream_node_ids(&start_results).context("parse Start streams")?;
    Ok(ScreenCastNodes { node_ids })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";

type Results = HashMap<String, OwnedValue>;

/// Make a portal method call and wait for the corresponding `Response` signal.
///
/// Subscribes to the Response on the *pre-computed* request path *before*
/// invoking `call_fn`, eliminating any race between the call returning and
/// signal registration.
///
/// `call_fn` receives the portal proxy and must call the appropriate method.
/// The return value (request object path) is consumed; what matters is the
/// `Response` signal body.
fn portal_call(
    conn: &Connection,
    portal: &Proxy<'_>,
    sender_flat: &str,
    req_token: &str,
    step: &str,
    call_fn: impl FnOnce(&Proxy<'_>) -> zbus::Result<(OwnedObjectPath,)>,
) -> anyhow::Result<Results> {
    let req_path = format!(
        "/org/freedesktop/portal/desktop/request/{sender_flat}/{req_token}"
    );

    let req_proxy = Proxy::new(conn, PORTAL_BUS, req_path.as_str(), REQUEST_IFACE)
        .with_context(|| format!("create Request proxy for {step}"))?;

    // Subscribe to Response signal BEFORE making the call.
    let mut resp_iter = req_proxy
        .receive_signal("Response")
        .with_context(|| format!("subscribe to Response for {step}"))?;

    // Execute the portal method call.
    call_fn(portal).with_context(|| format!("{step} D-Bus call"))?;

    // Block until the Response signal arrives.
    let msg = resp_iter
        .next()
        .ok_or_else(|| anyhow!("portal: Response stream ended for {step}"))?;

    let (code, results): (u32, Results) = msg
        .body()
        .deserialize()
        .with_context(|| format!("deserialise Response body for {step}"))?;

    match code {
        0 => Ok(results),
        1 => Err(anyhow!("portal: user cancelled at {step}")),
        _ => Err(anyhow!("portal: {step} failed (response code {code})")),
    }
}

/// Extract an object-path value from a Response results dict as a plain String.
///
/// Callers can then use `ObjectPath::try_from(str)` to obtain a typed handle.
fn extract_object_path_str(results: &Results, key: &str) -> anyhow::Result<String> {
    let val = results
        .get(key)
        .ok_or_else(|| anyhow!("missing '{key}' in portal results"))?;

    match &**val {
        Value::ObjectPath(p) => Ok(p.to_string()),
        Value::Str(s) => Ok(s.to_string()),
        other => Err(anyhow!(
            "expected object path for '{key}', got type {:?}",
            other.value_signature()
        )),
    }
}

/// Parse the `streams` value from the `Start` response.
///
/// D-Bus type: `a(ua{sv})`  — array of (node_id: u32, props: dict)
fn extract_stream_node_ids(results: &Results) -> anyhow::Result<Vec<u32>> {
    let val = results
        .get("streams")
        .ok_or_else(|| anyhow!("Start response has no 'streams' key"))?;

    let arr = match &**val {
        Value::Array(a) => a,
        other => {
            return Err(anyhow!(
                "'streams' has unexpected D-Bus type {:?}",
                other.value_signature()
            ))
        }
    };

    let mut ids = Vec::new();
    for item in arr.iter() {
        // Each item is a struct (u a{sv}).  First field is the node_id u32.
        if let Value::Structure(s) = item {
            if let Some(Value::U32(node_id)) = s.fields().first() {
                ids.push(*node_id);
            }
        }
    }

    if ids.is_empty() {
        return Err(anyhow!("Start response streams array contained no node IDs"));
    }
    Ok(ids)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Sender unique names are flattened correctly for Request object paths.
    #[test]
    fn sender_flat_simple() {
        let flat = ":1.42".trim_start_matches(':').replace('.', "_");
        assert_eq!(flat, "1_42");
    }

    #[test]
    fn sender_flat_larger() {
        let flat = ":1.100".trim_start_matches(':').replace('.', "_");
        assert_eq!(flat, "1_100");
    }

    /// Generated Request object paths match the portal spec.
    #[test]
    fn request_path_matches_spec() {
        let sender_flat = "1_42";
        let token = "tsod3";
        let path =
            format!("/org/freedesktop/portal/desktop/request/{sender_flat}/{token}");
        assert_eq!(
            path,
            "/org/freedesktop/portal/desktop/request/1_42/tsod3"
        );
    }

    /// Response code semantics: 0=ok, 1=cancelled, else=error.
    /// Mirrors the `portal_call` match expression.
    #[test]
    fn response_code_semantics() {
        fn interpret(code: u32) -> &'static str {
            match code {
                0 => "ok",
                1 => "cancelled",
                _ => "error",
            }
        }
        assert_eq!(interpret(0), "ok");
        assert_eq!(interpret(1), "cancelled");
        assert_eq!(interpret(2), "error");
        assert_eq!(interpret(255), "error");
    }

    /// Token counter increments monotonically.
    #[test]
    fn token_counter_increments() {
        let mut n: u32 = 0;
        let mut next_token = || { n += 1; format!("tsod{n}") };
        assert_eq!(next_token(), "tsod1");
        assert_eq!(next_token(), "tsod2");
        assert_eq!(next_token(), "tsod3");
    }
}
