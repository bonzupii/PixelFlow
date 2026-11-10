//! A mock XDG Desktop Portal `ScreenCast` service on a private
//! D-Bus session bus. This proves the session lifecycle against the
//! protocol instead of a live compositor: capability negotiation
//! picks an advertised cursor mode, the selection response is read
//! before `Start`, and a session that fails to open is closed.
//!
//! The mock serves `org.freedesktop.portal.ScreenCast` with
//! configurable properties and responses, records every call, and
//! emits the `Request::Response` signals ashpd waits for. The test
//! runs its scenarios sequentially in one function because the
//! D-Bus address is process-global and ashpd caches its bus
//! connection for the life of the process.

use pixelflow_capture::SourceKind;
use pixelflow_wayland::PortalSession;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use zbus::{
    Connection, ObjectServer, OwnedFd, OwnedObjectPath, Variant, fdo, interface, message::Header,
};

/// The behavior the mock portal shows for the next scenario.
#[derive(Debug, Clone)]
struct MockConfig {
    /// Interface version the mock reports.
    version: u32,
    /// Source types the mock offers.
    source_types: u32,
    /// Cursor modes the mock advertises.
    cursor_modes: u32,
    /// Response code for `SelectSources`: 0 success, 1 cancelled.
    select_response: u32,
    /// Response code for `Start`: 0 success, 1 cancelled.
    start_response: u32,
    /// Serial the mock reports for the started stream, if any.
    serial: Option<u64>,
    /// Report the serial under the hyphenated `pipewire-serial`
    /// key a real portal wires, instead of the `pipewire_serial`
    /// key ashpd 0.14 reads.
    serial_hyphenated: bool,
    /// Emit the session `Closed` signal before answering
    /// `SelectSources`, as a portal that closed the session would.
    emit_closed_after_select: bool,
    /// Emit the session `Closed` signal before answering `Start`,
    /// as a portal that closed the session mid-start would.
    emit_closed_after_start: bool,
    /// Answer `Close` on the session with `UnknownMethod`, as a
    /// portal that already destroyed the session object would.
    session_gone_on_close: bool,
}

impl Default for MockConfig {
    fn default() -> MockConfig {
        MockConfig {
            version: 6,
            source_types: 3,
            cursor_modes: 7,
            select_response: 0,
            start_response: 0,
            serial: Some(987_654_321),
            serial_hyphenated: false,
            emit_closed_after_select: false,
            emit_closed_after_start: false,
            session_gone_on_close: false,
        }
    }
}

/// One `SelectSources` call, as the mock received it.
#[derive(Debug, Default, Clone)]
struct SelectRecord {
    cursor_mode: Option<u32>,
    persist_mode: Option<u32>,
    types: Option<u32>,
    multiple: Option<bool>,
}

/// What the mock recorded, for assertions.
#[derive(Debug, Default)]
struct MockRecord {
    selects: Vec<SelectRecord>,
    starts: usize,
    opens: usize,
    closes: Vec<String>,
    creates: usize,
}

/// The mock portal's shared state: the current behavior plus the
/// record of calls so far.
#[derive(Debug, Default)]
struct MockState {
    config: MockConfig,
    record: MockRecord,
}

fn lock_state(state: &Arc<Mutex<MockState>>) -> std::sync::MutexGuard<'_, MockState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The request handle ashpd chose, from the options it sent: the
/// request object path is built from the caller's bus unique name
/// and the handle token, exactly as ashpd builds it. The sender
/// comes from the message header, not this connection: the mock's
/// own unique name is a different name on the bus.
fn request_path(sender: &str, options: &HashMap<String, Variant<'_>>) -> Option<OwnedObjectPath> {
    let token = options
        .get("handle_token")
        .and_then(|value| String::try_from(value).ok())?;
    let unique = sender.trim_start_matches(':').replace('.', "_");
    OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/{unique}/{token}"
    ))
    .ok()
}

/// The session handle ashpd chose, from the options it sent.
fn session_path(sender: &str, options: &HashMap<String, Variant<'_>>) -> Option<OwnedObjectPath> {
    let token = options
        .get("session_handle_token")
        .and_then(|value| String::try_from(value).ok())?;
    let unique = sender.trim_start_matches(':').replace('.', "_");
    OwnedObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/session/{unique}/{token}"
    ))
    .ok()
}

/// The caller of a portal method, from the message header.
fn sender_name(header: &Header<'_>) -> Result<String, fdo::Error> {
    header
        .sender()
        .map(|name| name.as_str().to_string())
        .ok_or_else(|| fdo::Error::Failed("portal call had no sender".to_string()))
}

/// Emit a `Request::Response` signal on a request handle, the way
/// the portal answers an asynchronous call.
async fn emit_response(
    connection: &Connection,
    path: &OwnedObjectPath,
    code: u32,
    results: HashMap<String, Variant<'static>>,
) -> Result<(), fdo::Error> {
    connection
        .emit_signal(
            None::<()>,
            path.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        )
        .await
        .map_err(|error| fdo::Error::Failed(error.to_string()))
}

/// The mock `ScreenCast` portal.
struct MockScreencast {
    state: Arc<Mutex<MockState>>,
}

#[interface(name = "org.freedesktop.portal.ScreenCast")]
impl MockScreencast {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        lock_state(&self.state).config.version
    }

    #[zbus(property)]
    fn available_source_types(&self) -> u32 {
        lock_state(&self.state).config.source_types
    }

    #[zbus(property)]
    fn available_cursor_modes(&self) -> u32 {
        lock_state(&self.state).config.cursor_modes
    }

    async fn create_session(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        options: HashMap<String, Variant<'_>>,
    ) -> Result<OwnedObjectPath, fdo::Error> {
        let sender = sender_name(&header)?;
        let request = request_path(&sender, &options)
            .ok_or_else(|| fdo::Error::Failed("no handle token in CreateSession".to_string()))?;
        let session = session_path(&sender, &options).ok_or_else(|| {
            fdo::Error::Failed("no session handle token in CreateSession".to_string())
        })?;
        server
            .at(
                session.clone(),
                MockSession {
                    state: self.state.clone(),
                    path: session.to_string(),
                },
            )
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        lock_state(&self.state).record.creates += 1;
        let mut results = HashMap::new();
        results.insert(
            "session_handle".to_string(),
            Variant::new(session.to_string()),
        );
        emit_response(connection, &request, 0, results).await?;
        Ok(request)
    }

    async fn select_sources(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        session: OwnedObjectPath,
        options: HashMap<String, Variant<'_>>,
    ) -> Result<OwnedObjectPath, fdo::Error> {
        let sender = sender_name(&header)?;
        let request = request_path(&sender, &options)
            .ok_or_else(|| fdo::Error::Failed("no handle token in SelectSources".to_string()))?;
        let (code, emit_closed) = {
            let mut state = lock_state(&self.state);
            state.record.selects.push(SelectRecord {
                cursor_mode: options
                    .get("cursor_mode")
                    .and_then(|value| u32::try_from(value).ok()),
                persist_mode: options
                    .get("persist_mode")
                    .and_then(|value| u32::try_from(value).ok()),
                types: options
                    .get("types")
                    .and_then(|value| u32::try_from(value).ok()),
                multiple: options
                    .get("multiple")
                    .and_then(|value| bool::try_from(value).ok()),
            });
            (
                state.config.select_response,
                state.config.emit_closed_after_select,
            )
        };
        if emit_closed {
            connection
                .emit_signal(
                    None::<()>,
                    session.as_str(),
                    "org.freedesktop.portal.Session",
                    "Closed",
                    &HashMap::<String, Variant>::new(),
                )
                .await
                .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        }
        emit_response(connection, &request, code, HashMap::new()).await?;
        Ok(request)
    }

    async fn start(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        session: OwnedObjectPath,
        _parent_window: String,
        options: HashMap<String, Variant<'_>>,
    ) -> Result<OwnedObjectPath, fdo::Error> {
        let sender = sender_name(&header)?;
        let request = request_path(&sender, &options)
            .ok_or_else(|| fdo::Error::Failed("no handle token in Start".to_string()))?;
        let (code, serial, serial_hyphenated, emit_closed) = {
            let mut state = lock_state(&self.state);
            state.record.starts += 1;
            (
                state.config.start_response,
                state.config.serial,
                state.config.serial_hyphenated,
                state.config.emit_closed_after_start,
            )
        };
        if emit_closed {
            connection
                .emit_signal(
                    None::<()>,
                    session.as_str(),
                    "org.freedesktop.portal.Session",
                    "Closed",
                    &HashMap::<String, Variant>::new(),
                )
                .await
                .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        }
        let mut results = HashMap::new();
        if code == 0 {
            let mut properties: HashMap<String, Variant<'static>> = HashMap::new();
            properties.insert("size".to_string(), Variant::new((1920i32, 1080i32)));
            properties.insert("source_type".to_string(), Variant::new(2u32));
            if let Some(serial) = serial {
                // The hyphenated key is what a real portal wires;
                // the underscore key is what ashpd 0.14 reads.
                let key = if serial_hyphenated {
                    "pipewire-serial"
                } else {
                    "pipewire_serial"
                };
                properties.insert(key.to_string(), Variant::new(serial));
            }
            let streams = vec![(42u32, properties)];
            results.insert("streams".to_string(), Variant::new(streams));
        }
        emit_response(connection, &request, code, results).await?;
        Ok(request)
    }

    async fn open_pipe_wire_remote(
        &self,
        _session: OwnedObjectPath,
        _options: HashMap<String, Variant<'_>>,
    ) -> Result<OwnedFd, fdo::Error> {
        lock_state(&self.state).record.opens += 1;
        let file = std::fs::File::open("/dev/null")
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        Ok(OwnedFd::from(std::os::fd::OwnedFd::from(file)))
    }
}

/// The mock `Session` object: records `Close` calls.
struct MockSession {
    state: Arc<Mutex<MockState>>,
    path: String,
}

#[interface(name = "org.freedesktop.portal.Session")]
impl MockSession {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }

    async fn close(&self) -> Result<(), fdo::Error> {
        let mut state = lock_state(&self.state);
        state.record.closes.push(self.path.clone());
        if state.config.session_gone_on_close {
            return Err(fdo::Error::UnknownMethod(format!(
                "Object does not exist at path \"{}\"",
                self.path
            )));
        }
        Ok(())
    }
}

/// Open a portal session on a blocking thread with a timeout, so a
/// lifecycle bug that hangs shows up as a failed test instead of a
/// hung suite.
async fn open_with_timeout(
    source: SourceKind,
) -> Result<Result<PortalSession, pixelflow_wayland::WaylandError>, Box<dyn std::error::Error>> {
    let joined = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || PortalSession::open(source)),
    )
    .await?;
    Ok(joined?)
}

/// The mock portal runs on the D-Bus session bus: run this test
/// under `dbus-run-session` so it owns a private bus. Without a
/// session bus the test fails instead of skipping, so a plain
/// `cargo test` never reports a green mock test that exercised
/// nothing. When the portal name is already owned (a real desktop
/// session), the test still skips: it cannot prove anything about
/// the lifecycle there.
/// Close a portal session on a blocking thread with a timeout, so
/// a close that hangs shows up as a failed test instead of a hung
/// suite. Close runs `block_on` on the session runtime, so it must
/// not run on the async test runtime thread.
async fn close_with_timeout(
    session: PortalSession,
) -> Result<Result<(), pixelflow_wayland::WaylandError>, Box<dyn std::error::Error>> {
    let joined = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || session.close()),
    )
    .await?;
    Ok(joined?)
}

#[tokio::test]
async fn mock_portal_session_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
    let connection = match Connection::session().await {
        Ok(connection) => connection,
        Err(error) => {
            // A missing session bus fails the test outright: a
            // skipped mock test would report green while proving
            // nothing about the lifecycle. `clippy::panic` denies
            // `panic!`, so the failure is a returned error carrying
            // the message instead.
            return Err(format!(
                "mock portal: no D-Bus session bus ({error}); run under dbus-run-session to exercise the mock portal test"
            )
            .into());
        }
    };
    if let Err(error) = connection
        .request_name("org.freedesktop.portal.Desktop")
        .await
    {
        eprintln!(
            "mock portal: cannot own the portal name ({error}); skipping mock portal test, run under dbus-run-session to exercise it"
        );
        return Ok(());
    }
    let state = Arc::new(Mutex::new(MockState::default()));
    connection
        .object_server()
        .at(
            "/org/freedesktop/portal/desktop",
            MockScreencast {
                state: state.clone(),
            },
        )
        .await?;

    // Version 6, everything advertised: embedded is selected, the
    // persist option is sent, the serial comes back.
    let opened = open_with_timeout(SourceKind::Window).await??;
    assert_eq!(opened.node_id, 42, "node id from the mock stream");
    assert_eq!(
        opened.serial,
        Some(987_654_321),
        "serial from the mock stream"
    );
    assert_eq!(opened.size, Some((1920, 1080)), "size from the mock stream");
    assert_eq!(
        opened.portal_version, 6,
        "portal version read from the mock"
    );
    {
        let state = lock_state(&state);
        assert_eq!(state.record.selects.len(), 1, "one selection");
        let select = &state.record.selects[0];
        assert_eq!(select.cursor_mode, Some(2), "embedded was requested");
        assert_eq!(select.persist_mode, Some(0), "do-not-persist was sent");
        assert_eq!(select.types, Some(2), "window source was requested");
        assert_eq!(select.multiple, Some(false), "single source was requested");
        assert_eq!(state.record.starts, 1, "one start");
    }
    close_with_timeout(opened).await??;
    assert_eq!(
        lock_state(&state).record.closes.len(),
        1,
        "close reached the portal session"
    );

    // Only hidden advertised: the request carries hidden, never an
    // unadvertised mode.
    lock_state(&state).config.cursor_modes = 1;
    let opened = open_with_timeout(SourceKind::Window).await??;
    assert_eq!(
        lock_state(&state).record.selects[1].cursor_mode,
        Some(1),
        "hidden was requested when only hidden is advertised"
    );
    close_with_timeout(opened).await??;

    // Only metadata advertised: metadata is selected over hidden.
    lock_state(&state).config.cursor_modes = 4;
    let opened = open_with_timeout(SourceKind::Window).await??;
    assert_eq!(
        lock_state(&state).record.selects[2].cursor_mode,
        Some(4),
        "metadata was requested when only metadata is advertised"
    );
    close_with_timeout(opened).await??;

    // Nothing advertised: no cursor mode is sent at all, so the
    // portal default applies.
    lock_state(&state).config.cursor_modes = 0;
    let opened = open_with_timeout(SourceKind::Window).await??;
    assert_eq!(
        lock_state(&state).record.selects[3].cursor_mode,
        None,
        "no cursor mode was sent when none is advertised"
    );
    close_with_timeout(opened).await??;

    // Version 1: the cursor mode and persist options stay off the
    // wire, because both arrived in later interface versions.
    {
        let mut state = lock_state(&state);
        state.config.version = 1;
        state.config.cursor_modes = 7;
    }
    let opened = open_with_timeout(SourceKind::Window).await??;
    {
        let state = lock_state(&state);
        assert_eq!(
            state.record.selects[4].cursor_mode, None,
            "no cursor mode was sent to a version 1 portal"
        );
        assert_eq!(
            state.record.selects[4].persist_mode, None,
            "no persist mode was sent to a version 1 portal"
        );
    }
    close_with_timeout(opened).await??;

    // No serial in the Start response: the session still opens, and
    // the caller falls back to the node id.
    {
        let mut state = lock_state(&state);
        state.config.version = 6;
        state.config.cursor_modes = 7;
        state.config.serial = None;
    }
    let opened = open_with_timeout(SourceKind::Window).await??;
    assert_eq!(opened.serial, None, "no serial was reported");
    assert_eq!(opened.node_id, 42, "node id still targets the stream");
    close_with_timeout(opened).await??;

    // A real portal wires the hyphenated serial key, which ashpd
    // 0.14 does not parse: the serial stays None and the node id
    // is the target, exactly as against a real portal today.
    {
        let mut state = lock_state(&state);
        state.config.serial = Some(987_654_321);
        state.config.serial_hyphenated = true;
    }
    let opened = open_with_timeout(SourceKind::Window).await??;
    assert_eq!(
        opened.serial, None,
        "the hyphenated serial key is not parsed by ashpd 0.14"
    );
    assert_eq!(opened.node_id, 42, "node id still targets the stream");
    close_with_timeout(opened).await??;

    // Selection cancelled: opening fails naming the selection
    // response, Start is never called, and the partial session is
    // closed on the portal side. The portal already destroyed the
    // session object when it cancelled, so that close answers
    // UnknownMethod; the original failure is still the one
    // reported, with the portal-side causes named.
    let closes_before = lock_state(&state).record.closes.len();
    let starts_before = lock_state(&state).record.starts;
    {
        let mut state = lock_state(&state);
        state.config.serial = Some(987_654_321);
        state.config.serial_hyphenated = false;
        state.config.select_response = 1;
        state.config.session_gone_on_close = true;
    }
    let failed = open_with_timeout(SourceKind::Window).await?;
    assert!(failed.is_err(), "cancelled selection must fail the open");
    if let Err(error) = failed {
        let text = error.to_string();
        assert!(
            text.contains("select sources response"),
            "the failure names the selection response: {text}"
        );
        assert!(
            text.contains("cancelled"),
            "the failure says the portal cancelled: {text}"
        );
        assert!(
            text.contains("source picker dialog was dismissed"),
            "the failure names the picker dismissal as a possible cause: {text}"
        );
        assert!(
            text.contains("compositor backend cancelled the request"),
            "the failure names the backend cancellation as a possible cause: {text}"
        );
    }
    assert_eq!(
        lock_state(&state).record.starts,
        starts_before,
        "Start was never called after a refused selection"
    );
    assert_eq!(
        lock_state(&state).record.closes.len(),
        closes_before + 1,
        "the partial session was closed after the refused selection"
    );

    // The portal closes the session after selection: opening fails
    // naming the closure, and Start is never called.
    {
        let mut state = lock_state(&state);
        state.config.select_response = 0;
        state.config.session_gone_on_close = false;
        state.config.emit_closed_after_select = true;
    }
    let starts_before = lock_state(&state).record.starts;
    let failed = open_with_timeout(SourceKind::Window).await?;
    assert!(failed.is_err(), "a closed session must fail the open");
    if let Err(error) = failed {
        let text = error.to_string();
        assert!(
            text.contains("closed by the portal"),
            "the failure names the portal closure: {text}"
        );
    }
    assert_eq!(
        lock_state(&state).record.starts,
        starts_before,
        "Start was never called after the portal closed the session"
    );

    // The portal closes the session after Start succeeds: opening
    // fails naming the closure, and the PipeWire remote is never
    // opened on the closed session.
    let closes_before = lock_state(&state).record.closes.len();
    let opens_before = lock_state(&state).record.opens;
    {
        let mut state = lock_state(&state);
        state.config.emit_closed_after_select = false;
        state.config.emit_closed_after_start = true;
        state.config.start_response = 0;
    }
    let failed = open_with_timeout(SourceKind::Window).await?;
    assert!(failed.is_err(), "a closed session must fail the open");
    if let Err(error) = failed {
        let text = error.to_string();
        assert!(
            text.contains("closed by the portal"),
            "the failure names the portal closure: {text}"
        );
        assert!(
            text.contains("after start"),
            "the failure names the closure happening after start: {text}"
        );
    }
    assert_eq!(
        lock_state(&state).record.opens,
        opens_before,
        "the PipeWire remote was never opened on the closed session"
    );
    assert_eq!(
        lock_state(&state).record.closes.len(),
        closes_before + 1,
        "the partial session was closed after the closure after start"
    );

    // Start cancelled: opening fails naming the start response, and
    // the partial session is closed.
    let closes_before = lock_state(&state).record.closes.len();
    {
        let mut state = lock_state(&state);
        state.config.emit_closed_after_select = false;
        state.config.emit_closed_after_start = false;
        state.config.start_response = 1;
    }
    let failed = open_with_timeout(SourceKind::Window).await?;
    assert!(failed.is_err(), "a cancelled start must fail the open");
    if let Err(error) = failed {
        let text = error.to_string();
        assert!(
            text.contains("read start response"),
            "the failure names the start response: {text}"
        );
    }
    assert_eq!(
        lock_state(&state).record.closes.len(),
        closes_before + 1,
        "the partial session was closed after the cancelled start"
    );

    drop(connection);
    Ok(())
}
