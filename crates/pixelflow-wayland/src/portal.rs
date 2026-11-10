//! XDG Desktop Portal `ScreenCast` session: the D-Bus calls that ask
//! the compositor for a capture stream, and the `PipeWire` remote fd
//! that comes back. The compositor owns source selection: it shows
//! its own dialog and `PixelFlow` records whatever window or monitor
//! the user picks.

use crate::WaylandError;
use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{PersistMode, Session};
use pixelflow_capture::SourceKind;
use std::os::fd::OwnedFd;

/// A started portal session: the `PipeWire` remote fd plus the stream
/// the compositor selected. Owns the tokio runtime the portal
/// connection runs on; dropping the runtime would strand the D-Bus
/// connection, so the runtime lives as long as the session.
pub struct PortalSession {
    runtime: tokio::runtime::Runtime,
    session: Session<Screencast>,
    /// The fd the portal handed out for the `PipeWire` remote. Taken
    /// once by the `PipeWire` stream via `take_fd`.
    fd: Option<OwnedFd>,
    /// The `PipeWire` node id of the selected stream. This is the
    /// connect target for the `PipeWire` stream.
    pub node_id: u32,
    /// The `PipeWire` serial of the selected stream, when the portal
    /// reports one. The `PipeWire` stream connects by node id; the
    /// serial is recorded for diagnostics.
    pub serial: Option<u64>,
    /// The size the portal reports for the selected stream, when it
    /// reports one. The negotiated `PipeWire` format is authoritative
    /// and may differ.
    pub size: Option<(u32, u32)>,
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

impl PortalSession {
    /// Create a session, request the source kind, and start the
    /// capture. Blocks until the user answers the compositor dialog
    /// or the request fails.
    ///
    /// # Errors
    ///
    /// Returns `Portal` when the D-Bus or portal call fails (no
    /// portal on this machine, dialog denied), `SourceUnavailable`
    /// when the compositor does not offer the requested source kind,
    /// and `NoStream` when the started session returns no stream.
    pub fn open(source: SourceKind) -> Result<PortalSession, WaylandError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| WaylandError::Portal(format!("build portal runtime: {e}")))?;
        let opened = runtime.block_on(async { Self::open_async(source).await })?;
        Ok(PortalSession {
            runtime,
            session: opened.session,
            fd: Some(opened.fd),
            node_id: opened.node_id,
            serial: opened.serial,
            size: opened.size,
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
        let session = proxy
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|e| portal_error("create session", &e))?;
        let options = SelectSourcesOptions::default()
            .set_cursor_mode(CursorMode::EMBEDDED)
            .set_sources(want)
            .set_multiple(false)
            .set_persist_mode(PersistMode::DoNot);
        proxy
            .select_sources(&session, options)
            .await
            .map_err(|e| portal_error("select sources", &e))?;
        let streams = proxy
            .start(&session, None, StartCastOptions::default())
            .await
            .map_err(|e| portal_error("start session", &e))?
            .response()
            .map_err(|e| portal_error("read start response", &e))?;
        let stream = streams.streams().first().ok_or(WaylandError::NoStream)?;
        let fd = proxy
            .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(|e| portal_error("open pipewire remote", &e))?;
        let size = stream.size().and_then(|(w, h)| {
            let w = u32::try_from(w).ok()?;
            let h = u32::try_from(h).ok()?;
            Some((w, h))
        });
        Ok(Opened {
            session,
            fd,
            node_id: stream.pipe_wire_node_id(),
            serial: stream.pipewire_serial(),
            size,
        })
    }

    /// Close the portal session. The compositor stops the stream.
    ///
    /// # Errors
    ///
    /// Returns `Portal` when the close call itself fails.
    pub fn close(self) -> Result<(), WaylandError> {
        self.runtime.block_on(async {
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
}
