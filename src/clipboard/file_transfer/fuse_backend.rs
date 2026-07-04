//! FUSE File Transfer Backend
//!
//! On-demand virtual filesystem for clipboard file transfer.
//! Files appear instantly as virtual entries; data is fetched from
//! Windows via RDP only when a Linux application reads the file.
//!
//! # FUSE Robustness
//!
//! - PID-unique mount paths: `$XDG_RUNTIME_DIR/lamco-clipboard-fuse-{PID}`
//! - Stale mount cleanup on startup via `/proc/self/mountinfo` parsing
//! - Automatic unmount on drop via fuser `BackgroundSession`
//! - Signal handler integration via server shutdown path
#![expect(unsafe_code, reason = "libc::getuid for FUSE mount path generation")]

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use async_trait::async_trait;
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_server::ServerEvent;
use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use super::{
    FileTransferBackend, FileTransferEvent, OutgoingFileInfo, PrepareResult, TransferFileDescriptor,
};
use crate::clipboard::{
    error::{ClipboardError, Result},
    fuse::{FileContentsRequest, FileContentsResponse, FileDescriptor, FuseMount},
};

/// FUSE file transfer backend.
///
/// Creates a virtual FUSE filesystem where clipboard files appear as
/// regular files. When a Linux application reads a file, the read()
/// call blocks while we fetch the data from Windows via RDP.
pub struct FuseFileTransfer {
    /// The FUSE mount manager
    fuse_mount: Option<FuseMount>,

    /// Mount point path (PID-unique)
    mount_point: PathBuf,

    /// Channel for sending file content requests from FUSE to async RDP
    fuse_request_tx: mpsc::Sender<FileContentsRequest>,

    /// Pending FUSE responses: stream_id → oneshot sender
    /// When FUSE read() triggers a FileContentsRequest, we store the
    /// response channel here. When the RDP response arrives, we send
    /// the data through the oneshot, unblocking the FUSE read().
    pending_fuse_responses: Arc<RwLock<HashMap<u32, oneshot::Sender<FileContentsResponse>>>>,

    /// Outgoing files (Linux → Windows)
    outgoing_files: RwLock<Vec<OutgoingFileInfo>>,

    /// Stream ID allocator
    next_stream_id: AtomicU32,

    /// Event channel (FUSE doesn't need async completion events,
    /// but the trait requires it)
    #[expect(
        dead_code,
        reason = "FUSE is synchronous; channel only kept alive so event_rx remains valid"
    )]
    event_tx: mpsc::UnboundedSender<FileTransferEvent>,
    event_rx: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<FileTransferEvent>>>,

    /// Server event sender for FUSE request handler.
    ///
    /// `Arc<RwLock<…>>` so the request-handler task (spawned at `initialize()`,
    /// before any client) holds a live handle and observes the sender once
    /// `prepare_files` stores it on connect — a one-time clone would freeze
    /// `None` and make every on-demand read fail with "RDP not connected".
    server_event_sender: Arc<RwLock<Option<mpsc::UnboundedSender<ServerEvent>>>>,

    /// Handle to the FUSE request handler background task
    request_handler_handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Shutdown signal for the request handler
    shutdown_tx: tokio::sync::Mutex<Option<tokio::sync::broadcast::Sender<()>>>,
}

impl Default for FuseFileTransfer {
    fn default() -> Self {
        Self::new()
    }
}

impl FuseFileTransfer {
    pub fn new() -> Self {
        let mount_point = get_pid_unique_mount_point();
        let (fuse_request_tx, _fuse_request_rx) = mpsc::channel::<FileContentsRequest>(32);
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            fuse_mount: None,
            mount_point,
            fuse_request_tx,
            pending_fuse_responses: Arc::new(RwLock::new(HashMap::new())),
            outgoing_files: RwLock::new(Vec::new()),
            next_stream_id: AtomicU32::new(1),
            event_tx,
            event_rx: parking_lot::Mutex::new(Some(event_rx)),
            server_event_sender: Arc::new(RwLock::new(None)),
            request_handler_handle: tokio::sync::Mutex::new(None),
            shutdown_tx: tokio::sync::Mutex::new(None),
        }
    }
}

/// Get a PID-unique mount point path.
///
/// Format: `$XDG_RUNTIME_DIR/lamco-clipboard-fuse-{PID}`
///
/// Each server process gets its own mount point. This eliminates
/// stale mount collisions — if a previous instance crashed, its
/// mount point has a different PID suffix.
fn get_pid_unique_mount_point() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    let pid = std::process::id();
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));

    PathBuf::from(runtime_dir).join(format!("lamco-clipboard-fuse-{pid}"))
}

/// Clean up stale FUSE mounts from crashed server instances.
///
/// Reads `/proc/self/mountinfo` to find mounts matching our naming
/// pattern, checks if the PID in the path is still alive, and calls
/// `fusermount3 -u` (or `fusermount -u`) on dead ones.
fn cleanup_stale_mounts() {
    let uid = unsafe { libc::getuid() };
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));

    let mountinfo = match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(s) => s,
        Err(e) => {
            debug!("Cannot read /proc/self/mountinfo: {}", e);
            return;
        }
    };

    for line in mountinfo.lines() {
        // mountinfo fields: mount_id parent_id major:minor root mount_point ...
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let mount_point = fields[4];

        // Only process our mounts in the runtime dir
        if !mount_point.starts_with(&runtime_dir) {
            continue;
        }
        if !mount_point.contains("lamco-clipboard-fuse-") {
            continue;
        }

        // Extract PID from the suffix: lamco-clipboard-fuse-{PID}
        let Some(pid_str) = mount_point.rsplit('-').next() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };

        // Skip our own PID
        if pid == std::process::id() {
            continue;
        }

        // Check if the process is still alive
        let proc_path = format!("/proc/{pid}");
        if std::path::Path::new(&proc_path).exists() {
            // PID exists — but is it actually our server? Check comm to guard
            // against PID recycling.
            let comm_path = format!("/proc/{pid}/comm");
            if let Ok(comm) = std::fs::read_to_string(&comm_path) {
                let comm = comm.trim();
                if comm.contains("lamco") || comm.contains("rdp-server") {
                    debug!(
                        "Stale mount check: PID {pid} is still a lamco process, skipping {mount_point}"
                    );
                    continue;
                }
                // PID recycled to a different process — this mount is stale
                info!("PID {pid} recycled (now '{comm}'), cleaning stale mount: {mount_point}");
            } else {
                // Can't read comm — process exists but we can't verify. Skip to be safe.
                debug!("Cannot read /proc/{pid}/comm, skipping stale check for {mount_point}");
                continue;
            }
        } else {
            info!("Cleaning up stale FUSE mount from dead PID {pid}: {mount_point}");
        }

        // Try fusermount3 first, fall back to fusermount
        let unmount_result = std::process::Command::new("fusermount3")
            .args(["-u", mount_point])
            .output();

        match unmount_result {
            Ok(output) if output.status.success() => {
                info!("Unmounted stale FUSE: {mount_point}");
            }
            _ => {
                // Try fusermount (FUSE 2.x fallback)
                let fallback = std::process::Command::new("fusermount")
                    .args(["-u", mount_point])
                    .output();
                match fallback {
                    Ok(output) if output.status.success() => {
                        info!("Unmounted stale FUSE (fusermount fallback): {mount_point}");
                    }
                    _ => {
                        warn!(
                            "Failed to unmount stale FUSE at {mount_point} — manual cleanup needed"
                        );
                        continue;
                    }
                }
            }
        }

        // Remove the empty directory
        let _ = std::fs::remove_dir(mount_point);
    }
}

#[async_trait]
impl FileTransferBackend for FuseFileTransfer {
    fn name(&self) -> &'static str {
        "FUSE On-Demand"
    }

    fn requires_eager_download(&self) -> bool {
        false
    }

    async fn initialize(&mut self) -> Result<()> {
        // Clean up stale mounts from crashed instances
        cleanup_stale_mounts();

        // Create the request channel (we need both ends)
        let (fuse_request_tx, fuse_request_rx) = mpsc::channel::<FileContentsRequest>(32);
        self.fuse_request_tx = fuse_request_tx;

        // Create and mount the FUSE filesystem
        match FuseMount::with_mount_point(self.mount_point.clone(), self.fuse_request_tx.clone()) {
            Ok(mut fm) => {
                if let Err(e) = fm.mount() {
                    warn!("FUSE mount failed: {:?}", e);
                    return Err(ClipboardError::FuseError(format!("Mount failed: {e}")));
                }
                info!("FUSE file transfer initialized at {:?}", self.mount_point);
                self.fuse_mount = Some(fm);
            }
            Err(e) => {
                return Err(ClipboardError::FuseError(format!(
                    "FuseMount creation failed: {e}"
                )));
            }
        }

        // Start the FUSE request handler background task
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        *self.shutdown_tx.lock().await = Some(shutdown_tx.clone());

        self.start_fuse_request_handler(fuse_request_rx, shutdown_tx.subscribe())
            .await;

        Ok(())
    }

    async fn prepare_files(
        &self,
        descriptors: &[TransferFileDescriptor],
        _portal_serial: u32,
        server_event_sender: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<PrepareResult> {
        let fuse_mount = match &self.fuse_mount {
            Some(fm) if fm.is_mounted() => fm,
            _ => {
                return Ok(PrepareResult::Failed("FUSE not mounted".to_string()));
            }
        };

        // Store the server event sender for the request handler
        *self.server_event_sender.write() = Some(server_event_sender.clone());

        // Lock emission removed (D.4.1, 2026-05-16). Upstream IronRDP's #1166
        // moved Lock/Unlock into Cliprdr's automatic state machine — see the
        // parallel comment in staging_backend.rs::start_transfer for the full
        // contract. clip_data_id is still required for FUSE-internal file
        // tracking (independent of the wire Lock PDU).
        let clip_data_id = descriptors.first().map_or(1, |d| d.clip_data_id);
        info!(
            clip_data_id,
            "FUSE transfer prepared; Cliprdr handles wire Lock/Unlock automatically"
        );

        // Convert descriptors to FUSE FileDescriptors
        let fuse_descriptors: Vec<FileDescriptor> = descriptors
            .iter()
            .map(|d| FileDescriptor::new(d.filename.clone(), d.size))
            .collect();

        // Create virtual files in FUSE
        let paths = fuse_mount.set_files(fuse_descriptors, Some(clip_data_id));

        if paths.is_empty() {
            return Ok(PrepareResult::Failed(
                "FUSE failed to create virtual files".to_string(),
            ));
        }

        info!(
            "FUSE: created {} virtual file(s) — available for on-demand read",
            paths.len()
        );

        Ok(PrepareResult::Ready(paths))
    }

    async fn deliver_file_data(&self, stream_id: u32, data: Vec<u8>, is_error: bool) -> Result<()> {
        // Deliver to the pending FUSE read() via oneshot channel
        match self.pending_fuse_responses.write().remove(&stream_id) {
            Some(response_tx) => {
                let response = if is_error {
                    FileContentsResponse::Error("RDP error".to_string())
                } else {
                    FileContentsResponse::Data(data)
                };

                if response_tx.send(response).is_err() {
                    warn!("FUSE response channel closed for stream_id={}", stream_id);
                } else {
                    trace!("Delivered FUSE response for stream_id={}", stream_id);
                }
            }
            None => {
                trace!(
                    "No pending FUSE request for stream_id={} (may be non-FUSE transfer)",
                    stream_id
                );
            }
        }
        Ok(())
    }

    async fn handle_outgoing_request(
        &self,
        stream_id: u32,
        list_index: u32,
        position: u64,
        requested_size: u32,
        is_size_request: bool,
        server_event_sender: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<()> {
        use ironrdp_cliprdr::pdu::FileContentsResponse as CliprdrFileContentsResponse;

        let outgoing = self.outgoing_files.read();
        let file_info = outgoing.get(list_index as usize).ok_or_else(|| {
            ClipboardError::InvalidState(format!("File index {list_index} not found"))
        })?;

        if is_size_request {
            let response =
                CliprdrFileContentsResponse::new_size_response(stream_id, file_info.size);
            let _ = server_event_sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsResponse(response),
            ));
        } else {
            let path = file_info.path.clone();
            drop(outgoing);

            use std::io::{Read, Seek, SeekFrom};
            match std::fs::File::open(&path) {
                Ok(mut file) => {
                    let _ = file.seek(SeekFrom::Start(position));
                    let mut buffer = vec![0u8; requested_size as usize];
                    let bytes_read = file.read(&mut buffer).unwrap_or(0);
                    buffer.truncate(bytes_read);

                    let response =
                        CliprdrFileContentsResponse::new_data_response(stream_id, buffer);
                    let _ = server_event_sender.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    ));
                }
                Err(e) => {
                    error!("Failed to read file '{}': {}", path.display(), e);
                    let response = CliprdrFileContentsResponse::new_error(stream_id);
                    let _ = server_event_sender.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    ));
                }
            }
        }

        Ok(())
    }

    fn set_outgoing_files(&self, files: Vec<OutgoingFileInfo>) {
        *self.outgoing_files.write() = files;
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<FileTransferEvent> {
        #[expect(
            clippy::expect_used,
            reason = "single-subscribe contract; calling twice is a programmer error"
        )]
        self.event_rx
            .lock()
            .take()
            .expect("subscribe() called more than once")
    }

    fn allocate_stream_id(&self) -> u32 {
        self.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn health_check(&self) -> Result<()> {
        if self.fuse_mount.as_ref().is_some_and(FuseMount::is_mounted) {
            Ok(())
        } else {
            Err(ClipboardError::FuseError("FUSE not mounted".to_string()))
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("FUSE file transfer shutting down");

        // Signal the request handler to stop
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }

        // Wait for the request handler task
        if let Some(handle) = self.request_handler_handle.lock().await.take() {
            let _ = handle.await;
        }

        // Unmount FUSE (BackgroundSession::drop does this, but be explicit)
        if let Some(mut fm) = self.fuse_mount.take()
            && let Err(e) = fm.unmount()
        {
            warn!("FUSE unmount error: {:?}", e);
        }

        // Remove the mount point directory
        let _ = std::fs::remove_dir(&self.mount_point);

        Ok(())
    }
}

impl FuseFileTransfer {
    /// Start the background task that bridges synchronous FUSE read()
    /// calls to async RDP FileContentsRequest/Response.
    async fn start_fuse_request_handler(
        &self,
        mut fuse_request_rx: mpsc::Receiver<FileContentsRequest>,
        mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    ) {
        let pending_responses = self.pending_fuse_responses.read().len();
        debug!(
            "Starting FUSE request handler ({} pending)",
            pending_responses
        );

        // Share the sender cell (not a snapshot) so the task reads it fresh per
        // request and picks up the sender prepare_files stores once a client
        // connects — the task starts at init, when the sender is still None.
        let server_event_sender = Arc::clone(&self.server_event_sender);
        let pending_fuse_responses = Arc::clone(&self.pending_fuse_responses);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(request) = fuse_request_rx.recv() => {
                        // Read the live sender per request (clone out, drop the
                        // guard before any send): None only if no client is
                        // currently connected.
                        let sender = match server_event_sender.read().as_ref() {
                            Some(s) => s.clone(),
                            None => {
                                let _ = request.response_tx.send(
                                    FileContentsResponse::Error("RDP not connected".to_string())
                                );
                                continue;
                            }
                        };

                        // Store the response channel and use file_index as stream_id
                        let stream_id = {
                            let id = request.file_index;
                            pending_fuse_responses.write().insert(id, request.response_tx);
                            id
                        };

                        use ironrdp_cliprdr::pdu::{
                            FileContentsFlags,
                            FileContentsRequest as CliprdrRequest,
                        };

                        let _ = sender.send(ServerEvent::Clipboard(
                            ClipboardMessage::SendFileContentsRequest(CliprdrRequest {
                                stream_id,
                                index: request.file_index as i32,
                                flags: FileContentsFlags::RANGE,
                                position: request.offset,
                                requested_size: request.size,
                                // None → upstream auto-fills from current_lock_id.
                                // Cliprdr emits Lock automatically when the file
                                // format is detected; downstream no longer needs
                                // to track or supply clip_data_id.
                                data_id: None,
                            }),
                        ));
                    }
                    _ = shutdown_rx.recv() => {
                        info!("FUSE request handler received shutdown signal");
                        break;
                    }
                }
            }
            info!("FUSE request handler stopped");
        });

        *self.request_handler_handle.lock().await = Some(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pid_unique_mount_point_contains_pid() {
        let path = get_pid_unique_mount_point();
        let pid = std::process::id();
        assert!(
            path.to_string_lossy()
                .contains(&format!("lamco-clipboard-fuse-{pid}")),
            "Mount point should contain PID: {path:?}"
        );
    }

    #[test]
    fn test_pid_unique_mount_point_in_runtime_dir() {
        let path = get_pid_unique_mount_point();
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("/run/user/") || path_str.contains("XDG_RUNTIME"),
            "Mount point should be in runtime dir: {path:?}"
        );
    }

    #[test]
    fn test_fuse_backend_new() {
        let backend = FuseFileTransfer::new();
        assert_eq!(backend.name(), "FUSE On-Demand");
        assert!(!backend.requires_eager_download());
    }

    #[test]
    fn test_stale_cleanup_doesnt_panic() {
        // Just verify it runs without panicking
        // (won't actually find stale mounts in test environment)
        cleanup_stale_mounts();
    }
}
