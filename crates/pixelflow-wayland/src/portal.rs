//! XDG Desktop Portal `ScreenCast` session: the D-Bus calls that ask
//! the compositor for a capture stream, and the `PipeWire` remote fd
//! that comes back. The compositor owns source selection: it shows
//! its own dialog and `PixelFlow` records whatever window or monitor
//! the user picks.
//!
//! The session lifecycle follows the portal specification in order:
//! create the session, negotiate capabilities against the advertised
//! properties and interface version, select sources exactly once and
//! read the selection response before doing anything else, start the
//! session exactly once, open the `PipeWire` remote. A session the
//! portal closes is never started, and a session that fails to open
//! is closed instead of dropped: dropping an ashpd `Session` does
//! not close the portal session, so every failure path after
//! creation closes it explicitly.

use crate::WaylandError;
use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions, Stream,
};
use ashpd::desktop::{PersistMode, Session};
use futures_util::{FutureExt, StreamExt};
use pixelflow_capture::SourceKind;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::OnceLock;

/// The tokio runtime portal work runs on. One runtime serves the
/// whole process: ashpd caches its D-Bus connection process-wide,
/// and that connection's tasks run on the runtime that created it,
/// so a fresh runtime per session would strand the cached
/// connection once the first session's runtime was dropped and
/// every later open would hang. A multi-thread runtime keeps the
/// connection driven between calls and lets opens and closes run
/// from any thread. Only a successfully built runtime is cached:
/// a failed build is reported and retried on the next call, never
/// stored, so one transient failure cannot fail every later open.
static PORTAL_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// The shared portal runtime, built on first use.
///
/// # Errors
///
/// Returns `Portal` when the runtime cannot be built.
fn portal_runtime() -> Result<&'static tokio::runtime::Runtime, WaylandError> {
    if let Some(runtime) = PORTAL_RUNTIME.get() {
        return Ok(runtime);
    }
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Ok(PORTAL_RUNTIME.get_or_init(|| runtime)),
        Err(error) => Err(WaylandError::Portal(format!(
            "build portal runtime: {error}"
        ))),
    }
}

/// A started portal session: the `PipeWire` remote fd plus the stream
/// the compositor selected.
pub struct PortalSession {
    session: Session<Screencast>,
    /// The fd the portal handed out for the `PipeWire` remote. Taken
    /// once by the `PipeWire` stream via `take_fd`.
    fd: Option<OwnedFd>,
    /// The `PipeWire` node id of the selected stream. This is the
    /// connect target for the `PipeWire` stream when the portal
    /// reports no serial.
    pub node_id: u32,
    /// The `PipeWire` serial of the selected stream, when the portal
    /// reports one. The `PipeWire` stream connects by serial when it
    /// is present; node ids can be reused after node destruction.
    /// Note the serial only surfaces through ashpd 0.14 when the
    /// stream dict carries the `pipewire_serial` key: the portal
    /// specification wires `pipewire-serial`, which that ashpd
    /// version does not parse, so against a real portal this is
    /// usually `None` and the node id is the target. The serial
    /// path still engages whenever a serial is reported.
    pub serial: Option<u64>,
    /// The size the portal reports for the selected stream, when it
    /// reports one. The negotiated `PipeWire` format is authoritative
    /// and may differ.
    pub size: Option<(u32, u32)>,
    /// The portal interface version the session negotiated against.
    /// Cursor modes need version 2, persistence needs version 4,
    /// and the serial needs version 6.
    pub portal_version: u32,
    /// The cursor mode requested in `SelectSources`, or `None` when
    /// no mode was sent and the portal default applies.
    pub cursor_mode: Option<CursorMode>,
}

fn portal_source(source: SourceKind) -> SourceType {
    match source {
        SourceKind::Window => SourceType::WINDOW,
        SourceKind::Monitor => SourceType::MONITOR,
    }
}

fn offered_sources(flags: SourceType) -> Vec<SourceKind> {
    let mut out = Vec::new();
    if flags.contains(SourceType::WINDOW) {
        out.push(SourceKind::Window);
    }
    if flags.contains(SourceType::MONITOR) {
        out.push(SourceKind::Monitor);
    }
    out
}

/// The pieces of an opened session, without the runtime. Built inside
/// `block_on` on the runtime the caller keeps.
struct Opened {
    session: Session<Screencast>,
    fd: OwnedFd,
    node_id: u32,
    serial: Option<u64>,
    size: Option<(u32, u32)>,
    portal_version: u32,
    cursor_mode: Option<CursorMode>,
}

/// The rest of an opened session, without the session itself: the
/// session stays with the caller so a failed run can close it.
struct SessionOutcome {
    fd: OwnedFd,
    node_id: u32,
    serial: Option<u64>,
    size: Option<(u32, u32)>,
}

/// Poll the `Closed` signal once without waiting. True when the
/// portal has already closed the session, if a subscription exists.
/// The stream borrows the session, so this stays generic over its
/// concrete type instead of boxing it.
fn closed_signalled<S>(closed: &mut Option<S>) -> bool
where
    S: futures_util::Stream<Item = HashMap<String, ashpd::zbus::wire::OwnedVariant>> + Unpin,
{
    let Some(stream) = closed.as_mut() else {
        return false;
    };
    stream.next().now_or_never().flatten().is_some()
}

/// Pick the cursor mode to request from the modes the portal
/// advertises. Preference is embedded, then metadata, then hidden:
/// embedded keeps the cursor in the captured frames, metadata keeps
/// it available without drawing it, hidden drops it. Returns `None`
/// when the portal advertises nothing, so the caller omits the
/// option and the portal default (hidden) applies instead of
/// submitting a mode the portal never offered: the specification
/// closes the session on an unadvertised mode.
fn select_cursor_mode(available: CursorMode) -> Option<CursorMode> {
    if available.contains(CursorMode::EMBEDDED) {
        Some(CursorMode::EMBEDDED)
    } else if available.contains(CursorMode::METADATA) {
        Some(CursorMode::METADATA)
    } else if available.contains(CursorMode::HIDDEN) {
        Some(CursorMode::HIDDEN)
    } else {
        None
    }
}

/// Format an ashpd error with the underlying D-Bus error name and
/// message. `ashpd::Error::Portal` displays through zbus's `DBusError`
/// derive as `{name}: {description}`, and for the `ZBus` catch-all
/// the description is the generic "FDO error": the real D-Bus error
/// name and message inside the `fdo::Error` are lost. Formatting the
/// inner `zbus::Error` directly prints the `fdo::Error` display,
/// which is the real name and message.
fn format_ashpd_error(error: &ashpd::Error) -> String {
    match error {
        ashpd::Error::Portal(ashpd::PortalError::ZBus(inner)) => inner.to_string(),
        other => other.to_string(),
    }
}

/// Wrap an ashpd error as a portal error, naming the call that
/// failed so the message says where the session setup broke.
fn portal_error(context: &str, error: &ashpd::Error) -> WaylandError {
    WaylandError::Portal(format!("{context}: {}", format_ashpd_error(error)))
}

/// Whether a failed session close means the portal already
/// destroyed the session object. The portal tears the session
/// down when a request is cancelled, so closing it then answers
/// `UnknownMethod` or `UnknownObject` for a path that no longer
/// exists. That is expected cleanup, not a second failure, so
/// the caller logs it as such and still reports the original
/// failure. The message check is the fallback for a portal that
/// reports the same condition under another error name.
fn session_already_gone(error: &ashpd::Error) -> bool {
    if let ashpd::Error::Portal(ashpd::PortalError::ZBus(ashpd::zbus::Error::FDO(inner))) = error
        && matches!(
            inner.as_ref(),
            ashpd::zbus::fdo::Error::UnknownMethod(_) | ashpd::zbus::fdo::Error::UnknownObject(_)
        )
    {
        return true;
    }
    format_ashpd_error(error).contains("does not exist at path")
}

/// Map a failed Start response read to the error the caller sees.
/// A cancellation at Start is portal-side, the same as at
/// `SelectSources`: backends that show their chooser at Start fail
/// here when the dialog is dismissed. The code cannot tell
/// dismissal from a backend cancellation, so the error says both
/// and asserts neither.
fn start_response_error(error: &ashpd::Error) -> WaylandError {
    if matches!(
        error,
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
    ) {
        WaylandError::Portal(format!(
            "read start response: {}; the portal cancelled the request: the source picker dialog was dismissed, or the compositor backend cancelled the request",
            format_ashpd_error(error)
        ))
    } else {
        portal_error("read start response", error)
    }
}

/// Verify the started stream is the requested source kind. The
/// compositor owns source selection and shows its own dialog, so
/// the stream it returns can be a different source kind than the
/// one requested: the user picks another source, or the backend
/// substitutes one. The run must not silently record the wrong
/// source, so a stream whose type differs from the request fails
/// the open naming both kinds. A type the code does not map to a
/// source kind fails the same way, naming the raw type instead.
fn verify_stream_source(
    stream: &Stream,
    requested: SourceKind,
    want: SourceType,
    handle: &str,
) -> Result<(), WaylandError> {
    let actual_type = stream.source_type();
    if actual_type == Some(want) {
        return Ok(());
    }
    let actual = match actual_type {
        Some(actual) if actual == SourceType::WINDOW => Some(SourceKind::Window),
        Some(actual) if actual == SourceType::MONITOR => Some(SourceKind::Monitor),
        _ => None,
    };
    Err(match actual {
        Some(actual) => WaylandError::SourceMismatch { requested, actual },
        None => WaylandError::Portal(format!(
            "session {handle}: requested {requested} capture but the portal returned a stream with source type {actual_type:?}"
        )),
    })
}

impl PortalSession {
    /// Create a session, request the source kind, and start the
    /// capture. Blocks until the user answers the compositor dialog
    /// or the request fails.
    ///
    /// # Errors
    ///
    /// Returns `Portal` when the D-Bus or portal call fails (no
    /// portal on this machine, dialog denied, selection refused, or
    /// the portal closed the session before it started),
    /// `SourceUnavailable` when the compositor does not offer the
    /// requested source kind, `SourceMismatch` when the started
    /// session returns a stream of another source kind, and
    /// `NoStream` when the started session returns no stream.
    pub fn open(source: SourceKind) -> Result<PortalSession, WaylandError> {
        let opened = portal_runtime()?.block_on(async { Self::open_async(source).await })?;
        Ok(PortalSession {
            session: opened.session,
            fd: Some(opened.fd),
            node_id: opened.node_id,
            serial: opened.serial,
            size: opened.size,
            portal_version: opened.portal_version,
            cursor_mode: opened.cursor_mode,
        })
    }

    /// Take the `PipeWire` remote fd. Returns `None` if it was already
    /// taken; the fd is handed to the `PipeWire` stream exactly once.
    #[must_use]
    pub fn take_fd(&mut self) -> Option<OwnedFd> {
        self.fd.take()
    }

    async fn open_async(source: SourceKind) -> Result<Opened, WaylandError> {
        let proxy = Screencast::new()
            .await
            .map_err(|e| portal_error("create portal proxy", &e))?;
        let version = proxy.version();
        let bus_name = proxy
            .connection()
            .unique_name()
            .map(|name| name.as_str().to_string());
        let available = proxy
            .available_source_types()
            .await
            .map_err(|e| portal_error("read available source types", &e))?;
        let want = portal_source(source);
        if !available.contains(want) {
            return Err(WaylandError::SourceUnavailable {
                requested: source,
                available: offered_sources(available),
            });
        }
        // The cursor mode option and its property arrived in
        // interface version 2. Below that, no mode is sent and the
        // portal default applies. A failed read also falls back to
        // the default: omitting the option is always valid.
        let advertised_modes = if version >= 2 {
            match proxy.available_cursor_modes().await {
                Ok(modes) => Some(modes),
                Err(error) => {
                    eprintln!(
                        "pixelflow: portal: could not read available cursor modes, using the portal default: {}",
                        format_ashpd_error(&error)
                    );
                    None
                }
            }
        } else {
            None
        };
        let cursor_mode = advertised_modes.and_then(select_cursor_mode);
        eprintln!(
            "pixelflow: portal: ScreenCast version {version}, bus {bus_name:?}, source {source} (offered {available:?}), cursor modes advertised {advertised_modes:?}, selected {cursor_mode:?}"
        );
        let session = proxy
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|e| portal_error("create session", &e))?;
        let handle = format!("{session:?}");
        eprintln!("pixelflow: portal: session {handle} created");
        match Self::run_session(
            &proxy,
            &session,
            &handle,
            source,
            want,
            cursor_mode,
            version,
        )
        .await
        {
            Ok(outcome) => Ok(Opened {
                session,
                fd: outcome.fd,
                node_id: outcome.node_id,
                serial: outcome.serial,
                size: outcome.size,
                portal_version: version,
                cursor_mode,
            }),
            Err(error) => {
                // The session is closed on every failure path after
                // creation. Dropping it would leak it on the portal
                // side, and the close itself can fail when the
                // portal already closed the session; either way the
                // original failure is the one reported.
                match session.close().await {
                    Ok(()) => eprintln!(
                        "pixelflow: portal: session {handle} closed after failure: {error}"
                    ),
                    Err(close_error) => {
                        if session_already_gone(&close_error) {
                            eprintln!(
                                "pixelflow: portal: session {handle} already closed by the portal; original failure: {error}"
                            );
                        } else {
                            eprintln!(
                                "pixelflow: portal: session {handle} close after failure also failed: {}; original failure: {error}",
                                format_ashpd_error(&close_error)
                            );
                        }
                    }
                }
                Err(error)
            }
        }
    }

    /// Select sources, start the session, and open the `PipeWire`
    /// remote. The session is the caller's; this returns only the
    /// outcome so the caller can close the session on failure.
    async fn run_session(
        proxy: &Screencast,
        session: &Session<Screencast>,
        handle: &str,
        requested: SourceKind,
        want: SourceType,
        cursor_mode: Option<CursorMode>,
        version: u32,
    ) -> Result<SessionOutcome, WaylandError> {
        // The Closed stream borrows the session, so it lives here
        // as a local. A failed subscription fails the open: the
        // signal is the only way the code learns the portal closed
        // the session before Start, so a session without it could
        // be started after the portal already closed it.
        let mut closed = Some(
            session
                .receive_closed()
                .await
                .map_err(|e| portal_error("subscribe to Closed signal", &e))?,
        );
        let mut options = SelectSourcesOptions::default()
            .set_sources(want)
            .set_multiple(false);
        if let Some(mode) = cursor_mode {
            options = options.set_cursor_mode(mode);
        }
        // The persist option arrived in interface version 4. Do not
        // is the default, so omitting it below version 4 changes
        // nothing and keeps an unknown option off the wire.
        if version >= 4 {
            options = options.set_persist_mode(PersistMode::DoNot);
        }
        let selection = proxy
            .select_sources(session, options)
            .await
            .map_err(|e| portal_error("select sources", &e))?;
        // The request future only resolves once the portal answered,
        // and the answer is stored in the request: it must be read.
        // Dropping the request unread swallows a refused or
        // cancelled selection, and the following Start then fails on
        // a session the portal already closed. A cancellation is
        // portal-side: the source picker dialog was dismissed, or
        // the compositor backend cancelled the request. The code
        // cannot tell which, so the error says both and asserts
        // neither.
        selection.response().map_err(|e| {
            if matches!(
                e,
                ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
            ) {
                WaylandError::Portal(format!(
                    "select sources response: {}; the source picker dialog was dismissed, or the compositor backend cancelled the request",
                    format_ashpd_error(&e)
                ))
            } else {
                portal_error("select sources response", &e)
            }
        })?;
        eprintln!("pixelflow: portal: session {handle}: source selection complete");
        if closed_signalled(&mut closed) {
            return Err(WaylandError::Portal(format!(
                "session {handle} was closed by the portal after source selection"
            )));
        }
        // A Closed signal can arrive between the check above and
        // the Start call below, so poll again immediately before
        // starting: starting a session the portal already closed
        // fails with a misleading error and wastes the dialog.
        if closed_signalled(&mut closed) {
            return Err(WaylandError::Portal(format!(
                "session {handle} was closed by the portal before start"
            )));
        }
        let start = match proxy
            .start(session, None, StartCastOptions::default())
            .await
        {
            Ok(request) => request,
            Err(error) => {
                let note = if closed_signalled(&mut closed) {
                    " (the portal had already closed the session)"
                } else {
                    ""
                };
                return Err(portal_error(&format!("start session{note}"), &error));
            }
        };
        let streams = start.response().map_err(|e| start_response_error(&e))?;
        // The portal can close the session while the Start response
        // is in flight, so check again before opening the PipeWire
        // remote: opening it on a closed session fails, and the
        // fd would belong to a session the portal already ended.
        if closed_signalled(&mut closed) {
            return Err(WaylandError::Portal(format!(
                "session {handle} was closed by the portal after start"
            )));
        }
        let count = streams.streams().len();
        let stream = streams.streams().first().ok_or(WaylandError::NoStream)?;
        verify_stream_source(stream, requested, want, handle)?;
        let node_id = stream.pipe_wire_node_id();
        let serial = stream.pipewire_serial();
        if node_id == 0 && serial.is_none() {
            return Err(WaylandError::Portal(format!(
                "session {handle}: started stream has no PipeWire target (node id 0, no serial)"
            )));
        }
        let size = stream.size().and_then(|(w, h)| {
            let w = u32::try_from(w).ok()?;
            let h = u32::try_from(h).ok()?;
            Some((w, h))
        });
        eprintln!(
            "pixelflow: portal: session {handle}: started {count} stream(s): node id {node_id}, serial {serial:?}, size {size:?}, source type {:?}",
            stream.source_type()
        );
        let fd = proxy
            .open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(|e| portal_error("open pipewire remote", &e))?;
        eprintln!("pixelflow: portal: session {handle}: PipeWire remote opened");
        Ok(SessionOutcome {
            fd,
            node_id,
            serial,
            size,
        })
    }

    /// Close the portal session. The compositor stops the stream.
    /// This takes `self` so a session cannot be used after close,
    /// and there is no `Drop` close: closing needs `block_on`, and
    /// a drop that runs inside an async runtime would panic on the
    /// nested `block_on` instead of closing. Callers close
    /// explicitly; every path in this crate does.
    ///
    /// # Errors
    ///
    /// Returns `Portal` when the close call itself fails.
    pub fn close(self) -> Result<(), WaylandError> {
        portal_runtime()?.block_on(async {
            self.session
                .close()
                .await
                .map_err(|e| portal_error("close session", &e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_mode_prefers_embedded() {
        let advertised = CursorMode::HIDDEN | CursorMode::EMBEDDED | CursorMode::METADATA;
        assert_eq!(select_cursor_mode(advertised), Some(CursorMode::EMBEDDED));
    }

    #[test]
    fn cursor_mode_falls_back_to_metadata() {
        let advertised = CursorMode::HIDDEN | CursorMode::METADATA;
        assert_eq!(select_cursor_mode(advertised), Some(CursorMode::METADATA));
    }

    #[test]
    fn cursor_mode_falls_back_to_hidden() {
        assert_eq!(
            select_cursor_mode(CursorMode::HIDDEN),
            Some(CursorMode::HIDDEN)
        );
    }

    #[test]
    fn cursor_mode_none_when_nothing_advertised() {
        assert_eq!(select_cursor_mode(CursorMode::empty()), None);
    }

    #[test]
    fn cursor_mode_selection_is_always_advertised() {
        let modes = [
            CursorMode::HIDDEN,
            CursorMode::EMBEDDED,
            CursorMode::METADATA,
        ];
        for mask in 0..8u32 {
            let mut advertised = CursorMode::empty();
            for (index, mode) in modes.iter().enumerate() {
                if mask & (1u32 << index) != 0 {
                    advertised |= *mode;
                }
            }
            if let Some(selected) = select_cursor_mode(advertised) {
                assert!(
                    advertised.contains(selected),
                    "selected {selected:?} from advertised {advertised:?}"
                );
            }
        }
    }

    #[test]
    fn portal_zbus_error_keeps_dbus_name_and_message() {
        let fdo =
            ashpd::zbus::fdo::Error::ServiceUnknown("the portal service is missing".to_string());
        let error = ashpd::Error::Portal(ashpd::PortalError::ZBus(ashpd::zbus::Error::FDO(
            Box::new(fdo),
        )));
        let formatted = format_ashpd_error(&error);
        assert!(
            formatted.contains("org.freedesktop.DBus.Error.ServiceUnknown"),
            "formatted: {formatted}"
        );
        assert!(
            formatted.contains("the portal service is missing"),
            "formatted: {formatted}"
        );
        assert!(
            !formatted.contains("FDO error"),
            "the generic description must not replace the real error: {formatted}"
        );
    }

    #[test]
    fn portal_failed_error_keeps_its_message() {
        let error = ashpd::Error::Portal(ashpd::PortalError::Failed("backend refused".to_string()));
        let formatted = format_ashpd_error(&error);
        assert!(
            formatted.contains("org.freedesktop.portal.Error.Failed"),
            "formatted: {formatted}"
        );
        assert!(
            formatted.contains("backend refused"),
            "formatted: {formatted}"
        );
    }

    #[test]
    fn portal_error_names_the_call() {
        let error =
            ashpd::Error::Portal(ashpd::PortalError::NotFound("no such session".to_string()));
        let err = portal_error("start session", &error);
        assert!(matches!(err, WaylandError::Portal(_)));
        let text = err.to_string();
        assert!(text.contains("start session"), "text: {text}");
        assert!(text.contains("NotFound"), "text: {text}");
    }

    fn fdo_portal_error(error: ashpd::zbus::fdo::Error) -> ashpd::Error {
        ashpd::Error::Portal(ashpd::PortalError::ZBus(ashpd::zbus::Error::FDO(Box::new(
            error,
        ))))
    }

    #[test]
    fn session_already_gone_true_for_unknown_method() {
        // The exact close error from the owner's log: the portal
        // destroyed the session object when the selection was
        // cancelled, so Close answers UnknownMethod for the path.
        let error = fdo_portal_error(ashpd::zbus::fdo::Error::UnknownMethod(
            "Object does not exist at path \"/org/freedesktop/portal/desktop/session/1_1611/ashpd_AgVQbTYZWC\""
                .to_string(),
        ));
        assert!(session_already_gone(&error));
    }

    #[test]
    fn session_already_gone_true_for_unknown_object() {
        let error = fdo_portal_error(ashpd::zbus::fdo::Error::UnknownObject(
            "Object does not exist at path \"/org/freedesktop/portal/desktop/session/1_1611/ashpd_AgVQbTYZWC\""
                .to_string(),
        ));
        assert!(session_already_gone(&error));
    }

    #[test]
    fn session_already_gone_true_for_the_message_alone() {
        let error =
            ashpd::Error::Portal(ashpd::PortalError::Failed(
                "Object does not exist at path \"/org/freedesktop/portal/desktop/session/1_1611/ashpd_AgVQbTYZWC\""
                    .to_string(),
            ));
        assert!(session_already_gone(&error));
    }

    #[test]
    fn session_already_gone_false_for_service_unknown() {
        let error = fdo_portal_error(ashpd::zbus::fdo::Error::ServiceUnknown(
            "the portal service is missing".to_string(),
        ));
        assert!(!session_already_gone(&error));
    }
}
