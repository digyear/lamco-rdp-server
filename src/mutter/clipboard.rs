//! Mutter Clipboard via RemoteDesktop D-Bus API
//!
//! Implements clipboard sharing using Mutter's RemoteDesktop session clipboard methods
//! (EnableClipboard, DisableClipboard, SetSelection, SelectionRead, SelectionWrite).
//!
//! This replaces the need for a Portal clipboard session when using the Mutter strategy.

use std::{collections::HashMap, io::Write};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use zbus::zvariant::{OwnedObjectPath, Value};

use super::remote_desktop::MutterRemoteDesktopSession;

/// Supported MIME types for clipboard sharing
const CLIPBOARD_MIME_TYPES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
    "text/html",
    "image/png",
    "image/bmp",
];

/// Mutter clipboard manager
///
/// Wraps the RemoteDesktop session's clipboard methods to provide clipboard
/// sharing without needing a separate Portal session.
pub struct MutterClipboard {
    /// D-Bus connection for creating session proxies
    connection: zbus::Connection,
    /// RemoteDesktop session object path. Interior-mutable so this manager can
    /// be re-pointed at a re-established session under the `PerConnection`
    /// lifecycle (see `rebind`) without recreating the provider that holds it.
    rd_session_path: std::sync::RwLock<OwnedObjectPath>,
    /// Whether clipboard is currently enabled
    enabled: Mutex<bool>,
    /// Monotonic session-establishment counter. Starts at 0 for the initial
    /// session and increments on every [`rebind`](Self::rebind). Signal listeners
    /// record the generation they last subscribed on; comparing that to the
    /// current value tells health whether a listener is bound to the live session
    /// or a dead one (see `MutterClipboardProvider::health_check`).
    session_generation: std::sync::atomic::AtomicU64,
    /// Fires once per `rebind`. The provider's signal listeners select on this to
    /// re-subscribe on the re-established session — because destroying a Mutter
    /// RemoteDesktop session does NOT end the zbus signal-match stream (it just
    /// goes silent), so a listener waiting on stream-end would never re-subscribe.
    rebind_tx: tokio::sync::broadcast::Sender<()>,
}

impl MutterClipboard {
    /// Create a new clipboard manager for the given RemoteDesktop session
    pub(crate) fn new(connection: zbus::Connection, rd_session_path: OwnedObjectPath) -> Self {
        let (rebind_tx, _) = tokio::sync::broadcast::channel(8);
        Self {
            connection,
            rd_session_path: std::sync::RwLock::new(rd_session_path),
            enabled: Mutex::new(false),
            session_generation: std::sync::atomic::AtomicU64::new(0),
            rebind_tx,
        }
    }

    /// Current session-establishment generation (0 = initial session).
    pub(crate) fn session_generation(&self) -> u64 {
        self.session_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Subscribe to `rebind` notifications. Each signal listener holds one of
    /// these and re-subscribes to its D-Bus stream whenever it fires.
    pub(crate) fn subscribe_rebind(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.rebind_tx.subscribe()
    }

    /// Re-point this clipboard at a re-established RemoteDesktop session.
    ///
    /// The `PerConnection` lifecycle recreates the compositor session on each
    /// reconnect; the wired clipboard provider holds this same `Arc`, so we
    /// update the session path in place and re-enable clipboard sharing (a fresh
    /// Mutter session starts with it disabled). Without this, `SetSelection` and
    /// friends target the old, destroyed session and fail after any reconnect.
    pub(crate) async fn rebind(&self, new_rd_session_path: OwnedObjectPath) -> Result<()> {
        info!(
            new_rd_session = %new_rd_session_path.as_str(),
            "[mutter-clipboard] Rebinding to re-established RemoteDesktop session"
        );
        *self
            .rd_session_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new_rd_session_path;
        // A freshly re-established session is created with clipboard already
        // enabled, so EnableClipboard then returns "Already enabled" — treat that
        // as success rather than a rebind failure.
        match self.enable().await {
            Ok(()) => {}
            // The full error chain (not just the top context) carries the D-Bus
            // reason, so format with `{:#}` to see "Already enabled".
            Err(e) if format!("{e:#}").contains("Already enabled") => {
                *self.enabled.lock().await = true;
            }
            Err(e) => return Err(e),
        }
        // Advance the generation and wake the signal listeners so they re-subscribe
        // on this session. Without this the listeners stay silently bound to the
        // destroyed session and copy/paste dies with no error on every reconnect.
        let new_generation = self
            .session_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        let woke = self.rebind_tx.send(()).unwrap_or(0);
        info!(
            generation = new_generation,
            listeners_woken = woke,
            "[mutter-clipboard] Rebound to re-established session; signalling listeners to re-subscribe"
        );
        Ok(())
    }

    /// Get a session proxy for D-Bus calls
    async fn session_proxy(&self) -> Result<MutterRemoteDesktopSession<'_>> {
        let path = self
            .rd_session_path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        MutterRemoteDesktopSession::new(&self.connection, path)
            .await
            .context("Failed to create session proxy for clipboard")
    }

    /// Enable clipboard sharing with Mutter
    pub(crate) async fn enable(&self) -> Result<()> {
        let proxy = self.session_proxy().await?;

        let mut options: HashMap<String, Value<'_>> = HashMap::new();
        // Advertise the MIME types we can handle.
        // Must be 'as' (array of strings), not 'av' (array of variants).
        // Mutter expects GVariant type 'as' per grd's serialize_mime_type_tables().
        let mime_types: Vec<String> = CLIPBOARD_MIME_TYPES
            .iter()
            .map(|&s| s.to_string())
            .collect();
        options.insert("mime-types".to_string(), Value::new(mime_types));

        proxy.enable_clipboard(options).await?;

        *self.enabled.lock().await = true;
        info!("Mutter clipboard enabled");
        Ok(())
    }

    /// Disable clipboard sharing
    pub(crate) async fn disable(&self) -> Result<()> {
        let proxy = self.session_proxy().await?;
        proxy.disable_clipboard().await?;

        *self.enabled.lock().await = false;
        info!("Mutter clipboard disabled");
        Ok(())
    }

    /// Check if clipboard is enabled
    pub(crate) async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    /// Advertise clipboard content ownership (Linux -> RDP direction)
    ///
    /// Call this when the local clipboard content changes and we want to
    /// advertise it to Mutter.
    pub(crate) async fn set_selection(&self, mime_types: &[String]) -> Result<()> {
        let proxy = self.session_proxy().await?;

        let mut options: HashMap<String, Value<'_>> = HashMap::new();
        // Must be 'as' (array of strings), not 'av' (array of variants).
        let types: Vec<String> = mime_types.to_vec();
        options.insert("mime-types".to_string(), Value::new(types));

        proxy.set_selection(options).await?;
        debug!(
            "Set clipboard selection with {} MIME types",
            mime_types.len()
        );
        Ok(())
    }

    /// Read clipboard content from Mutter for a given MIME type
    ///
    /// Returns the raw clipboard data bytes.
    pub(crate) async fn read_selection(&self, mime_type: &str) -> Result<Vec<u8>> {
        let proxy = self.session_proxy().await?;

        let fd = proxy.selection_read(mime_type).await?;

        let mime_owned = mime_type.to_string();

        // Read data from the FD with a timeout to avoid hanging forever.
        // The FD is a pipe from Mutter; the writing app may not close its end promptly.
        let read_task = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            use std::{io::Read as _, os::fd::AsRawFd};

            let raw_fd = fd.as_raw_fd();

            // Set non-blocking + use poll to read with timeout
            // This prevents hanging if the writing app doesn't close the pipe
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut file = std::fs::File::from(fd);

            // Set a read timeout by polling the FD
            let mut poll_fd = [libc::pollfd {
                fd: raw_fd,
                events: libc::POLLIN,
                revents: 0,
            }];

            loop {
                // Poll with 3-second timeout
                // SAFETY: poll_fd is a valid stack-allocated pollfd array, nfds=1 matches
                #[expect(unsafe_code, reason = "libc::poll requires unsafe for FFI call")]
                let poll_result = unsafe { libc::poll(poll_fd.as_mut_ptr(), 1, 3000) };

                if poll_result == 0 {
                    // Timeout -- return what we have
                    debug!(
                        "SelectionRead poll timeout after reading {} bytes (MIME: {})",
                        buf.len(),
                        mime_owned
                    );
                    break;
                }
                if poll_result < 0 {
                    return Err(anyhow::anyhow!(
                        "poll() failed on clipboard FD: {}",
                        std::io::Error::last_os_error()
                    ));
                }

                // Data available
                match file.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        return Err(anyhow::anyhow!(
                            "Failed to read clipboard data from FD: {e}"
                        ));
                    }
                }
            }
            Ok(buf)
        });

        let data = tokio::time::timeout(std::time::Duration::from_secs(5), read_task)
            .await
            .context("Clipboard read timed out (5s)")??
            .context("Clipboard read task failed")?;

        debug!(
            "Read {} bytes of clipboard data (MIME: {})",
            data.len(),
            mime_type
        );
        Ok(data)
    }

    /// Write clipboard content to Mutter (RDP -> Linux direction)
    ///
    /// Responds to a SelectionTransfer by writing data to the FD Mutter provides.
    pub(crate) async fn write_selection(&self, serial: u32, data: &[u8]) -> Result<()> {
        let proxy = self.session_proxy().await?;

        let fd = proxy.selection_write(serial).await?;

        // Write data to the FD, then signal completion BEFORE closing.
        // grd closes the FD after selection_write_done, not before.
        // Mutter needs the FD open when write_done is called.
        let data_owned = data.to_vec();
        let write_result = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let mut file = std::fs::File::from(fd);
            file.write_all(&data_owned)
                .context("Failed to write clipboard data to FD")?;
            // Return the File without dropping it (keeps FD open)
            Ok(file)
        })
        .await
        .context("Clipboard write task panicked")?;

        match write_result {
            Ok(file) => {
                // Signal completion while FD is still open
                proxy.selection_write_done(serial, true).await?;
                // Now close the FD by dropping the File
                drop(file);
                debug!(
                    "Wrote {} bytes to clipboard (serial: {})",
                    data.len(),
                    serial
                );
            }
            Err(e) => {
                warn!("Clipboard write failed: {}", e);
                proxy.selection_write_done(serial, false).await.ok();
            }
        }

        Ok(())
    }

    /// Subscribe to SelectionOwnerChanged signal
    ///
    /// This signal fires when another application claims clipboard ownership,
    /// meaning new content is available for us to read and forward to the RDP client.
    pub(crate) async fn subscribe_selection_owner_changed(
        &self,
    ) -> Result<impl futures_util::Stream<Item = zbus::Message> + use<>> {
        let proxy = self.session_proxy().await?;
        proxy.subscribe_selection_owner_changed().await
    }

    /// Subscribe to SelectionTransfer signal
    ///
    /// This signal fires when Mutter requests clipboard data from us
    /// (i.e., a local application wants to paste RDP client content).
    pub(crate) async fn subscribe_selection_transfer(
        &self,
    ) -> Result<impl futures_util::Stream<Item = zbus::Message> + use<>> {
        let proxy = self.session_proxy().await?;
        proxy.subscribe_selection_transfer().await
    }
}

impl std::fmt::Debug for MutterClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutterClipboard")
            .field(
                "rd_session_path",
                &*self
                    .rd_session_path
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_mime_types() {
        // Verify we have the essential MIME types
        assert!(CLIPBOARD_MIME_TYPES.contains(&"text/plain;charset=utf-8"));
        assert!(CLIPBOARD_MIME_TYPES.contains(&"text/plain"));
        assert!(CLIPBOARD_MIME_TYPES.contains(&"image/png"));
    }
}
