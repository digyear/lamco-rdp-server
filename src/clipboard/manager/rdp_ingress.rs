//! RDP-ingress clipboard handlers (CLIPRDR -> orchestrator)
//!
//! Split out of `manager/mod.rs` (architecture audit M3). These `impl`
//! blocks contribute to the same `ClipboardOrchestrator` type; `use super::*`
//! inherits the parent module's imports and private-field access.

use std::{collections::HashMap, sync::Arc};

use lamco_clipboard_core::{
    ClipboardFormat, FormatConverter, TransferEngine,
    sanitize::{parse_file_uris, sanitize_text_for_linux, sanitize_text_for_windows},
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, trace, warn};

use super::{
    ClipboardOrchestrator, ClipboardOrchestratorConfig, EAGER_FETCH_SERIAL, PendingPortalRequests,
    ServerEventSender, SharedClipboardProvider,
};
use crate::clipboard::{
    FormatConverterExt,
    error::{ClipboardError, Result},
    sync::SyncManager,
};

impl ClipboardOrchestrator {
    /// Handle RDP format list announcement
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator handler with shared state refs"
    )]
    pub(super) async fn handle_rdp_format_list(
        formats: Vec<ClipboardFormat>,
        converter: &FormatConverter,
        sync_manager: &Arc<RwLock<SyncManager>>,
        clipboard_provider: &SharedClipboardProvider,
        current_rdp_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
        config: &ClipboardOrchestratorConfig,
        klipper_info: &Arc<RwLock<crate::clipboard::klipper::KlipperInfo>>,
        cooperation_coordinator: &Arc<
            RwLock<Option<crate::clipboard::KlipperCooperationCoordinator>>,
        >,
        server_event_sender: &ServerEventSender,
        pending_portal_requests: &PendingPortalRequests,
        local_advertised_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
    ) -> Result<()> {
        debug!("RDP format list received: {:?}", formats);

        // Registered format IDs vary per session, store for later lookup
        {
            let mut stored_formats = current_rdp_formats.write().await;
            stored_formats.clone_from(&formats);
            debug!(
                "Stored {} RDP formats for format ID lookup",
                stored_formats.len()
            );
        }

        {
            let coordinator_opt = cooperation_coordinator.read().await;
            if let Some(ref coordinator) = *coordinator_opt {
                coordinator.update_rdp_formats(formats.clone()).await;
                debug!(
                    "Updated cooperation coordinator with {} RDP formats",
                    formats.len()
                );
            }
        }

        let should_sync = {
            let mut mgr = sync_manager.write().await;
            mgr.handle_rdp_formats(formats.clone())
        };

        if !should_sync {
            debug!("Skipping RDP format list due to loop detection");
            return Ok(());
        }

        let mut mime_types = converter.rdp_to_mime_types(&formats)?;

        debug!("Converted to MIME types: {:?}", mime_types);

        if mime_types.is_empty() {
            debug!("Empty format list from RDP client (handshake only, no clipboard content)");
            return Ok(());
        }

        // The remote now owns the Linux selection: its formats are about to be
        // announced to the compositor, superseding whatever a local app last
        // offered. Drop the stale local advertisement so a later RdpReady
        // re-announce can't claim we still hold formats the live selection no
        // longer has — that mismatch made SelectionRead fail and mstsc
        // retry-loop. A fresh local copy repopulates this via handle_portal_formats.
        local_advertised_formats.write().await.clear();

        if config.kde_syncselection_hint {
            let klipper_detected = {
                let info = klipper_info.read().await;
                info.detected && info.responsive
            };

            if klipper_detected {
                warn!("⚠️  EXPERIMENTAL: Adding x-kde-syncselection hint");
                warn!("   This tells Klipper to completely ignore our clipboard");
                warn!("   This MIME type is intended for Klipper's internal use only");

                const KDE_SYNCSELECTION: &str = "application/x-kde-syncselection";

                if !mime_types.contains(&KDE_SYNCSELECTION.to_string()) {
                    mime_types.push(KDE_SYNCSELECTION.to_string());
                    debug!("   Added {} to MIME types", KDE_SYNCSELECTION);
                }
            } else {
                debug!("kde_syncselection_hint enabled but Klipper not detected - skipping hint");
            }
        }

        debug!("Final MIME types for SetSelection: {:?}", mime_types);

        // Delayed rendering: announce format availability WITHOUT transferring data
        info!("┌─ SetSelection (RDP → Provider) ──────────────────────────────");
        info!(
            "│ Announcing {} MIME types: {:?}",
            mime_types.len(),
            mime_types
        );
        info!(
            "│ Echo protection window starts NOW ({}ms)",
            2000 // ECHO_PROTECTION_WINDOW_MS from sync.rs
        );
        info!("│ Any SelectionOwnerChanged within this window will be blocked");
        info!("└────────────────────────────────────────────────────────────────");

        // Announce via clipboard provider
        let provider_opt = clipboard_provider.read().await;
        if let Some(ref provider) = *provider_opt {
            provider
                .announce_formats(mime_types.clone())
                .await
                .map_err(|e| {
                    ClipboardError::PortalError(format!("Provider announce_formats failed: {e}"))
                })?;
            debug!(
                "RDP clipboard formats announced via {} provider",
                provider.name()
            );

            // Data-control path: Wayland `send` is synchronous, so data must be in memory
            // before the compositor requests it. Eagerly fetch text from the RDP client now.
            if provider.requires_upfront_data() {
                let has_text = mime_types.iter().any(|m| m.starts_with("text/plain"));
                let has_cf_unicodetext = formats.iter().any(|f| f.id == 13);

                if has_text && has_cf_unicodetext {
                    info!("Data-control provider: eagerly fetching CF_UNICODETEXT from RDP client");

                    // Queue a sentinel entry so handle_rdp_data_response knows this is eager fetch
                    pending_portal_requests.write().await.push_back((
                        EAGER_FETCH_SERIAL,
                        "text/plain".to_string(),
                        std::time::Instant::now(),
                    ));

                    let sender_opt = server_event_sender.read().await.clone();
                    if let Some(sender) = sender_opt {
                        use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::ClipboardFormatId};

                        if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                            ClipboardMessage::SendInitiatePaste(ClipboardFormatId(13)),
                        )) {
                            warn!("Failed to send eager fetch for CF_UNICODETEXT: {:?}", e);
                            // Remove the sentinel we just pushed
                            let mut pending = pending_portal_requests.write().await;
                            pending.retain(|(s, _, _)| *s != EAGER_FETCH_SERIAL);
                        }
                    } else {
                        warn!("ServerEvent sender not available for eager fetch");
                        pending_portal_requests
                            .write()
                            .await
                            .retain(|(s, _, _)| *s != EAGER_FETCH_SERIAL);
                    }
                }

                // Files: trigger the FileGroupDescriptorW paste so IronRDP fetches
                // the remote file list. IronRDP does not fetch it on FormatList; the
                // result arrives via `on_remote_file_list` → `RdpRemoteFileList`,
                // which pre-populates the eager data-control source with file URIs.
                // No pending-queue entry: the response is delivered through the
                // file-list callback, not the FIFO request queue.
                let has_file_mime = mime_types
                    .iter()
                    .any(|m| m == "text/uri-list" || m == "x-special/gnome-copied-files");
                if has_file_mime
                    && let Some(fgd_id) = formats
                        .iter()
                        .find(|f| f.name.as_deref() == Some("FileGroupDescriptorW"))
                        .map(|f| f.id)
                {
                    info!(
                        "Data-control provider: initiating FileGroupDescriptorW paste to fetch remote file list"
                    );
                    let sender_opt = server_event_sender.read().await.clone();
                    if let Some(sender) = sender_opt {
                        use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::ClipboardFormatId};

                        if let Err(e) = sender.send(ironrdp_server::ServerEvent::Clipboard(
                            ClipboardMessage::SendInitiatePaste(ClipboardFormatId(fgd_id)),
                        )) {
                            warn!("Failed to initiate FileGroupDescriptorW paste: {:?}", e);
                        }
                    }
                }
            }
        } else {
            debug!("No clipboard provider available (normal during startup)");
        }

        Ok(())
    }

    /// Handle RDP data request (Linux → Windows paste)
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator handler with shared state refs"
    )]
    pub(super) async fn handle_rdp_data_request(
        format_id: u32,
        converter: &FormatConverter,
        _sync_manager: &Arc<RwLock<SyncManager>>,
        clipboard_provider: &SharedClipboardProvider,
        server_event_sender: &ServerEventSender,
        local_advertised_formats: &Arc<RwLock<Vec<ClipboardFormat>>>,
        file_transfer_backend: &Arc<
            tokio::sync::RwLock<Box<dyn crate::clipboard::file_transfer::FileTransferBackend>>,
        >,
        cooperation_content_cache: &Arc<RwLock<Option<Vec<u8>>>>,
    ) -> Result<()> {
        info!(
            "RDP data request for format ID: {} (Linux → Windows paste)",
            format_id
        );

        // PRIORITY 1: Check cooperation content cache (from Klipper sync)
        // If we recently synced from Klipper, serve that content
        if let Some(cached_data) = cooperation_content_cache.read().await.as_ref() {
            // Check if format_id matches what we cached (CF_UNICODETEXT=13 or CF_TEXT=1)
            if format_id == 13 || format_id == 1 {
                info!(
                    "✅ Serving from cooperation cache: {} bytes (Klipper sync)",
                    cached_data.len()
                );

                let sender_opt = server_event_sender.read().await.clone();
                if let Some(sender) = sender_opt {
                    use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FormatDataResponse};
                    use ironrdp_pdu::IntoOwned;

                    let data_to_send = if format_id == 1 {
                        // CF_TEXT: client wants ANSI text, cache is UTF-16LE
                        let text = ironrdp_pdu::utils::from_utf16_bytes(cached_data);
                        let trimmed = text.trim_end_matches('\0');
                        let mut bytes = trimmed.as_bytes().to_vec();
                        bytes.push(0); // CF_TEXT null terminator
                        debug!(
                            "Converted cooperation cache UTF-16LE ({} bytes) to CF_TEXT ({} bytes)",
                            cached_data.len(),
                            bytes.len()
                        );
                        bytes
                    } else {
                        // CF_UNICODETEXT: cache is already UTF-16LE
                        cached_data.clone()
                    };

                    let response = FormatDataResponse::new_data(data_to_send.clone());
                    let owned_response = response.into_owned();

                    if sender
                        .send(ironrdp_server::ServerEvent::Clipboard(
                            ClipboardMessage::SendFormatData(owned_response),
                        ))
                        .is_ok()
                    {
                        info!(
                            "Sent {} bytes from cooperation cache to RDP client",
                            data_to_send.len()
                        );
                        return Ok(());
                    }
                } else {
                    warn!("ServerEvent sender not available");
                }
            }
        }

        // Normal path: read from Portal clipboard
        let advertised = local_advertised_formats.read().await;
        let format_name = advertised
            .iter()
            .find(|f| f.id == format_id || (format_id == 0 && f.name.is_some()))
            .and_then(|f| f.name.clone());
        drop(advertised);

        if let Some(ref name) = format_name
            && name == "FileGroupDescriptorW"
        {
            debug!(
                "Windows requests FileGroupDescriptorW - sending file list from Linux clipboard"
            );
            return Self::handle_file_descriptor_request(
                clipboard_provider,
                server_event_sender,
                file_transfer_backend,
            )
            .await;
        }

        let mime_type = match converter.format_id_to_mime(format_id) {
            Ok(m) => m,
            Err(e) => {
                warn!("Unknown format ID {}: {:?}", format_id, e);
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };
        debug!("Format {} maps to MIME: {}", format_id, mime_type);

        let portal_data = {
            let provider_opt = clipboard_provider.read().await;
            if let Some(ref provider) = *provider_opt {
                match provider.read_data(&mime_type).await {
                    Ok(data) => {
                        info!(
                            "Read {} bytes from {} provider ({})",
                            data.len(),
                            provider.name(),
                            mime_type
                        );
                        data
                    }
                    Err(e) => {
                        error!("Failed to read from {} provider: {:#}", provider.name(), e);
                        Self::send_format_data_error(server_event_sender).await;
                        return Ok(());
                    }
                }
            } else {
                warn!("No clipboard provider available for RDP data request");
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };

        let rdp_data = if format_id == 13 {
            // CF_UNICODETEXT - Convert UTF-8 to UTF-16LE with line ending conversion
            let text = String::from_utf8_lossy(&portal_data);
            // Sanitize text for Windows: LF → CRLF, remove null bytes
            let sanitized = sanitize_text_for_windows(&text);
            let utf16: Vec<u16> = sanitized.encode_utf16().collect();
            let mut bytes = Vec::with_capacity(utf16.len() * 2 + 2);
            for c in utf16 {
                bytes.extend_from_slice(&c.to_le_bytes());
            }
            bytes.extend_from_slice(&[0, 0]); // Null terminator
            debug!(
                "Converted UTF-8 ({} bytes) to UTF-16LE ({} bytes) with CRLF line endings",
                portal_data.len(),
                bytes.len()
            );
            bytes
        } else if format_id == 8 {
            // CF_DIB - Windows wants DIB, Portal has image format
            if mime_type.starts_with("image/png") {
                trace!(" Converting PNG to DIB for Windows");
                lamco_clipboard_core::image::png_to_dib(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else if mime_type.starts_with("image/jpeg") {
                trace!(" Converting JPEG to DIB for Windows");
                lamco_clipboard_core::image::jpeg_to_dib(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else if mime_type.starts_with("image/bmp") || mime_type.starts_with("image/x-bmp") {
                trace!(" Converting BMP to DIB for Windows");
                lamco_clipboard_core::image::bmp_to_dib(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else {
                debug!("Unknown image MIME for DIB: {}, passing through", mime_type);
                portal_data
            }
        } else if format_id == 17 {
            // CF_DIBV5 - Windows wants DIBV5 with alpha channel support
            if mime_type.starts_with("image/png") {
                trace!(" Converting PNG to DIBV5 for Windows (with alpha)");
                lamco_clipboard_core::image::png_to_dibv5(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else if mime_type.starts_with("image/jpeg") {
                trace!(" Converting JPEG to DIBV5 for Windows");
                lamco_clipboard_core::image::jpeg_to_dibv5(&portal_data)
                    .map_err(ClipboardError::Core)?
            } else {
                // Unsupported MIME for DIBV5, fall back to raw data
                debug!(
                    "Unknown image MIME for DIBV5: {}, passing through",
                    mime_type
                );
                portal_data
            }
        } else if format_id == 0xD011 {
            // CF_PNG - Windows wants PNG
            if mime_type.starts_with("image/png") {
                debug!("PNG to PNG - pass through");
                portal_data
            } else {
                debug!("Unsupported conversion to PNG from {}", mime_type);
                portal_data
            }
        } else {
            debug!(
                "Format {} - pass through {} bytes",
                format_id,
                portal_data.len()
            );
            portal_data
        };

        let data_len = rdp_data.len();
        debug!("Converted to RDP format: {} bytes", data_len);

        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FormatDataResponse};
            use ironrdp_pdu::IntoOwned;

            let response = FormatDataResponse::new_data(rdp_data);
            let owned_response = response.into_owned();

            match sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendFormatData(owned_response),
            )) {
                Err(e) => {
                    error!("Failed to send FormatDataResponse via ServerEvent: {:?}", e);
                }
                _ => {
                    info!(
                        "Sent {} bytes to RDP client for format {} (Linux → Windows)",
                        data_len, format_id
                    );
                }
            }
        } else {
            warn!("ServerEvent sender not available - cannot send clipboard data to RDP");
        }

        Ok(())
    }

    /// Pre-populate an eager clipboard source with file URIs when the remote
    /// sends its file list ([MS-RDPECLIP] 2.2.5.2), surfaced via IronRDP's
    /// `on_remote_file_list`. This is the Windows → Linux file paste path for
    /// synchronous data-control clipboards (e.g. COSMIC / `ext-data-control`),
    /// whose `send` handler serves from pre-loaded data and cannot fetch on
    /// demand. On-demand providers (GNOME/KDE) skip this and render lazily via
    /// their SelectionWrite path.
    pub(super) async fn handle_rdp_remote_file_list(
        files: Vec<lamco_rdp_clipboard::RemoteFileMetadata>,
        clip_data_id: Option<u32>,
        clipboard_provider: &SharedClipboardProvider,
        file_transfer_backend: &Arc<
            tokio::sync::RwLock<Box<dyn crate::clipboard::file_transfer::FileTransferBackend>>,
        >,
        server_event_sender: &ServerEventSender,
        transfer_data_cache: &Arc<RwLock<HashMap<String, Vec<u8>>>>,
    ) -> Result<()> {
        let provider_opt = clipboard_provider.read().await;
        let Some(provider) = provider_opt.as_ref() else {
            return Ok(());
        };
        if !provider.requires_upfront_data() || files.is_empty() {
            return Ok(());
        }

        use lamco_clipboard_core::sanitize::sanitize_filename_for_linux;

        use crate::clipboard::file_transfer::{
            PrepareResult, TransferFileDescriptor, generate_gnome_copied_files_content,
            generate_uri_list_content,
        };

        let transfer_descriptors: Vec<TransferFileDescriptor> = files
            .iter()
            .enumerate()
            .map(|(idx, f)| TransferFileDescriptor {
                filename: sanitize_filename_for_linux(&f.name),
                size: f.size.unwrap_or(0),
                file_index: idx as u32,
                clip_data_id: clip_data_id.unwrap_or(1),
            })
            .collect();

        let sender = match server_event_sender.read().await.as_ref() {
            Some(s) => s.clone(),
            None => {
                warn!("RemoteFileList: ServerEvent sender not available");
                return Ok(());
            }
        };

        let backend = file_transfer_backend.read().await;
        match backend
            .prepare_files(&transfer_descriptors, 0, &sender)
            .await?
        {
            PrepareResult::Ready(paths) => {
                let urilist_bytes = generate_uri_list_content(&paths).into_bytes();
                let gnome_bytes = generate_gnome_copied_files_content(&paths).into_bytes();
                if let Err(e) = provider
                    .provide_data("text/uri-list", urilist_bytes.clone())
                    .await
                {
                    warn!("RemoteFileList: provide text/uri-list failed: {e}");
                }
                if let Err(e) = provider
                    .provide_data("x-special/gnome-copied-files", gnome_bytes.clone())
                    .await
                {
                    warn!("RemoteFileList: provide gnome-copied-files failed: {e}");
                }
                info!(
                    "RemoteFileList: provided {} file URI(s) to eager clipboard source",
                    paths.len()
                );
                let mut cache = transfer_data_cache.write().await;
                cache.insert("text/uri-list".to_string(), urilist_bytes);
                cache.insert("x-special/gnome-copied-files".to_string(), gnome_bytes);
            }
            PrepareResult::Pending => {
                warn!("RemoteFileList: staging backend pending; eager source needs upfront data");
            }
            PrepareResult::Failed(reason) => {
                error!("RemoteFileList: prepare_files failed: {reason}");
            }
        }
        Ok(())
    }

    /// Handle FileGroupDescriptorW request from Windows (Linux → Windows file transfer)
    ///
    /// Reads file URIs from Portal clipboard and converts to Windows FILEDESCRIPTORW format.
    async fn handle_file_descriptor_request(
        clipboard_provider: &SharedClipboardProvider,
        server_event_sender: &ServerEventSender,
        file_transfer_backend: &Arc<
            tokio::sync::RwLock<Box<dyn crate::clipboard::file_transfer::FileTransferBackend>>,
        >,
    ) -> Result<()> {
        // Read file URIs: prefer x-special/gnome-copied-files, fall back to text/uri-list
        let uri_data = {
            let provider_opt = clipboard_provider.read().await;
            if let Some(ref provider) = *provider_opt {
                match provider.read_data("x-special/gnome-copied-files").await {
                    Ok(data) if !data.is_empty() => {
                        info!(
                            "Read {} bytes from {} provider (x-special/gnome-copied-files)",
                            data.len(),
                            provider.name()
                        );
                        data
                    }
                    _ => match provider.read_data("text/uri-list").await {
                        Ok(data) => {
                            info!(
                                "Read {} bytes from {} provider (text/uri-list)",
                                data.len(),
                                provider.name()
                            );
                            data
                        }
                        Err(e) => {
                            error!(
                                "Failed to read file URIs from {} provider: {:#}",
                                provider.name(),
                                e
                            );
                            Self::send_format_data_error(server_event_sender).await;
                            return Ok(());
                        }
                    },
                }
            } else {
                warn!("No clipboard provider available for file descriptor request");
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };

        let file_paths = parse_file_uris(&uri_data);

        for path in &file_paths {
            trace!("Found file: {:?}", path);
        }

        if file_paths.is_empty() {
            warn!("No valid file paths found in clipboard");
            Self::send_format_data_error(server_event_sender).await;
            return Ok(());
        }

        {
            use crate::clipboard::file_transfer::OutgoingFileInfo;

            let outgoing: Vec<OutgoingFileInfo> = file_paths
                .iter()
                .enumerate()
                .filter_map(|(idx, path)| {
                    std::fs::metadata(path).ok().map(|metadata| {
                        let filename = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                            .to_string();
                        OutgoingFileInfo {
                            list_index: idx as u32,
                            path: path.clone(),
                            size: metadata.len(),
                            filename,
                        }
                    })
                })
                .collect();

            let count = outgoing.len();
            let backend = file_transfer_backend.read().await;
            backend.set_outgoing_files(outgoing);
            info!("Stored {} outgoing files for transfer", count);
        }

        let descriptor_data = match lamco_rdp_clipboard::build_file_group_descriptor_w(&file_paths)
        {
            Ok(data) => {
                info!(
                    "Built FileGroupDescriptorW ({} bytes) for {} files",
                    data.len(),
                    file_paths.len()
                );
                data
            }
            Err(e) => {
                error!("Failed to build FileGroupDescriptorW: {:?}", e);
                Self::send_format_data_error(server_event_sender).await;
                return Ok(());
            }
        };

        let sender_opt = server_event_sender.read().await.clone();
        if let Some(sender) = sender_opt {
            use ironrdp_cliprdr::{backend::ClipboardMessage, pdu::FormatDataResponse};
            use ironrdp_pdu::IntoOwned;

            let response = FormatDataResponse::new_data(descriptor_data);
            let owned_response = response.into_owned();

            match sender.send(ironrdp_server::ServerEvent::Clipboard(
                ClipboardMessage::SendFormatData(owned_response),
            )) {
                Err(e) => {
                    error!("Failed to send FileGroupDescriptorW response: {:?}", e);
                }
                _ => {
                    debug!(" Sent FileGroupDescriptorW to Windows (Linux → Windows file transfer)");
                }
            }
        }

        Ok(())
    }

    /// Handle RDP data response (Windows → Linux paste completion)
    #[expect(
        clippy::too_many_arguments,
        reason = "orchestrator handler with shared state refs"
    )]
    #[expect(
        clippy::expect_used,
        reason = "provider existence verified by caller before this path"
    )]
    pub(super) async fn handle_rdp_data_response(
        data: Vec<u8>,
        sync_manager: &Arc<RwLock<SyncManager>>,
        _transfer_engine: &TransferEngine,
        clipboard_provider: &SharedClipboardProvider,
        pending_portal_requests: &PendingPortalRequests,
        file_transfer_backend: &Arc<
            tokio::sync::RwLock<Box<dyn crate::clipboard::file_transfer::FileTransferBackend>>,
        >,
        server_event_sender: &ServerEventSender,
        transfer_data_cache: &Arc<RwLock<HashMap<String, Vec<u8>>>>,
    ) -> Result<()> {
        debug!("RDP data response received: {} bytes", data.len());

        let should_transfer = sync_manager.write().await.check_content(&data, true);
        if !should_transfer {
            debug!("Skipping RDP data due to content loop detection");
            return Ok(());
        }

        let provider_opt = clipboard_provider.read().await.clone();
        if provider_opt.is_none() {
            warn!("No clipboard provider available - cannot deliver clipboard data");
            return Ok(());
        }

        // Get FIRST pending request (FIFO order)
        // IronRDP doesn't correlate requests/responses, so we use FIFO queue
        let mut pending = pending_portal_requests.write().await;
        let request_opt = pending.pop_front();
        drop(pending);

        let (serial, requested_mime, _request_time) = match request_opt {
            Some(req) => req,
            None => {
                warn!("No pending request - FormatDataResponse arrived with no matching request");
                return Ok(());
            }
        };

        info!(
            "Matched FormatDataResponse to serial {} (FIFO queue)",
            serial
        );
        debug!(
            "Requested MIME: {}, received {} bytes from Windows",
            requested_mime,
            data.len()
        );

        // Eager fetch for data-control: provide data upfront instead of completing a transfer
        if serial == EAGER_FETCH_SERIAL {
            let provider = provider_opt.as_ref().expect("provider checked above");

            // Convert UTF-16LE from CF_UNICODETEXT to UTF-8
            if data.len() >= 2 {
                let utf16_data: Vec<u16> = data
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .take_while(|&c| c != 0)
                    .collect();

                let text = String::from_utf16_lossy(&utf16_data);
                let sanitized = sanitize_text_for_linux(&text);
                let utf8_bytes = sanitized.as_bytes().to_vec();

                info!(
                    "Eager fetch: {} UTF-16 chars → {} UTF-8 bytes for data-control source",
                    utf16_data.len(),
                    utf8_bytes.len()
                );

                // Provide under both bare and charset-qualified MIME types so the
                // compositor finds data regardless of which key it requests via `send`
                if let Err(e) = provider
                    .provide_data("text/plain", utf8_bytes.clone())
                    .await
                {
                    warn!("Failed to provide eager-fetched text to data-control: {e}");
                }
                if let Err(e) = provider
                    .provide_data("text/plain;charset=utf-8", utf8_bytes)
                    .await
                {
                    warn!("Failed to provide eager-fetched text (charset): {e}");
                }
            } else {
                debug!(
                    "Eager fetch: data too small ({} bytes), skipping",
                    data.len()
                );
            }

            return Ok(());
        }

        // Special handling for file transfer formats
        if requested_mime == "text/uri-list" || requested_mime == "x-special/gnome-copied-files" {
            info!(
                "Received FileGroupDescriptorW data ({} bytes) - parsing file list",
                data.len()
            );

            match lamco_rdp_clipboard::FileDescriptor::parse_list(&data) {
                Ok(descriptors) => {
                    info!(
                        "Parsed {} file descriptor(s) from Windows",
                        descriptors.len()
                    );

                    for (idx, desc) in descriptors.iter().enumerate() {
                        info!(
                            "  File {}: {} ({} bytes)",
                            idx,
                            desc.name,
                            desc.size.unwrap_or(0)
                        );
                    }

                    use lamco_clipboard_core::sanitize::sanitize_filename_for_linux;

                    use crate::clipboard::file_transfer::{PrepareResult, TransferFileDescriptor};

                    let transfer_descriptors: Vec<TransferFileDescriptor> = descriptors
                        .iter()
                        .enumerate()
                        .map(|(idx, d)| TransferFileDescriptor {
                            filename: sanitize_filename_for_linux(&d.name),
                            size: d.size.unwrap_or(0),
                            file_index: idx as u32,
                            clip_data_id: 1,
                        })
                        .collect();

                    let sender = match server_event_sender.read().await.as_ref() {
                        Some(s) => s.clone(),
                        None => {
                            error!("ServerEvent sender not available");
                            if let Some(ref provider) = provider_opt {
                                let _ = provider
                                    .complete_transfer(serial, &requested_mime, vec![], false)
                                    .await;
                            }
                            return Ok(());
                        }
                    };

                    let backend = file_transfer_backend.read().await;
                    match backend
                        .prepare_files(&transfer_descriptors, serial, &sender)
                        .await?
                    {
                        PrepareResult::Ready(paths) => {
                            // GNOME announces both text/uri-list and x-special/gnome-copied-files
                            // and its clipboard-manager poll loop fires SelectionTransfers that
                            // race the user's actual paste. Unlike the generic-data path below,
                            // this branch used to answer only the serial that triggered the RDP
                            // fetch and return — leaving the paste's serial unanswered, so Files
                            // pasted nothing. Answer every pending file-MIME serial from the same
                            // data and cache both variants for GNOME's subsequent requests.
                            use crate::clipboard::file_transfer::{
                                generate_gnome_copied_files_content, generate_uri_list_content,
                            };
                            let gnome_bytes =
                                generate_gnome_copied_files_content(&paths).into_bytes();
                            let urilist_bytes = generate_uri_list_content(&paths).into_bytes();
                            let content_for = |mime: &str| match mime {
                                "text/uri-list" => Some(urilist_bytes.clone()),
                                "x-special/gnome-copied-files" => Some(gnome_bytes.clone()),
                                _ => None,
                            };

                            if let Some(ref provider) = provider_opt {
                                let first = content_for(&requested_mime)
                                    .unwrap_or_else(|| gnome_bytes.clone());
                                provider
                                    .complete_transfer(serial, &requested_mime, first, true)
                                    .await?;

                                let others: Vec<(u32, String)> = {
                                    let mut pending = pending_portal_requests.write().await;
                                    let drained = pending
                                        .iter()
                                        .filter(|(s, _, _)| *s != serial)
                                        .map(|(s, m, _)| (*s, m.clone()))
                                        .collect();
                                    pending.clear();
                                    drained
                                };
                                for (other_serial, other_mime) in others {
                                    let result = match content_for(&other_mime) {
                                        Some(bytes) => {
                                            provider
                                                .complete_transfer(
                                                    other_serial,
                                                    &other_mime,
                                                    bytes,
                                                    true,
                                                )
                                                .await
                                        }
                                        None => {
                                            provider
                                                .complete_transfer(
                                                    other_serial,
                                                    &other_mime,
                                                    vec![],
                                                    false,
                                                )
                                                .await
                                        }
                                    };
                                    if let Err(e) = result {
                                        warn!(
                                            "Failed to answer pending serial {other_serial}: {e}"
                                        );
                                    }
                                }
                            }

                            let mut cache = transfer_data_cache.write().await;
                            cache.insert("x-special/gnome-copied-files".to_string(), gnome_bytes);
                            cache.insert("text/uri-list".to_string(), urilist_bytes);
                        }
                        PrepareResult::Pending => {
                            // Staging backend will notify via event channel
                        }
                        PrepareResult::Failed(reason) => {
                            error!("File transfer backend failed: {}", reason);
                            if let Some(ref provider) = provider_opt {
                                let _ = provider
                                    .complete_transfer(serial, &requested_mime, vec![], false)
                                    .await;
                            }
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    error!("Failed to parse FileGroupDescriptorW: {:?}", e);
                    // Fall through to generic handling
                }
            }
        }

        let portal_data = if requested_mime.starts_with("image/png") {
            // Portal wants PNG, Windows sent DIB or DIBV5
            // Auto-detect format based on header size
            if data.len() >= 4 {
                let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                match header_size {
                    124 => {
                        // DIBV5 format with alpha channel
                        trace!(" Converting DIBV5 to PNG for Portal (with alpha)");
                        lamco_clipboard_core::image::dibv5_to_png(&data).map_err(|e| {
                            error!("DIBV5 to PNG conversion failed: {}", e);
                            ClipboardError::Core(e)
                        })?
                    }
                    40 => {
                        // Standard DIB format
                        trace!(" Converting DIB to PNG for Portal");
                        lamco_clipboard_core::image::dib_to_png(&data).map_err(|e| {
                            error!("DIB to PNG conversion failed: {}", e);
                            ClipboardError::Core(e)
                        })?
                    }
                    _ => {
                        // Unknown header size, try DIBV5 parser which handles both
                        debug!(
                            "Unknown bitmap header size {}, trying auto-detect",
                            header_size
                        );
                        lamco_clipboard_core::image::dibv5_to_png(&data).map_err(|e| {
                            error!("Bitmap to PNG conversion failed: {}", e);
                            ClipboardError::Core(e)
                        })?
                    }
                }
            } else {
                error!(
                    "Image data too small for bitmap header: {} bytes",
                    data.len()
                );
                return Err(ClipboardError::Core(
                    lamco_clipboard_core::ClipboardError::ImageDecode(
                        "Data too small for bitmap".to_string(),
                    ),
                ));
            }
        } else if requested_mime.starts_with("image/jpeg") {
            // Portal wants JPEG, Windows sent DIB or DIBV5
            if data.len() >= 4 {
                let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                if header_size == 124 {
                    trace!(" Converting DIBV5 to JPEG for Portal");
                    lamco_clipboard_core::image::dibv5_to_jpeg(&data).map_err(|e| {
                        error!("DIBV5 to JPEG conversion failed: {}", e);
                        ClipboardError::Core(e)
                    })?
                } else {
                    trace!(" Converting DIB to JPEG for Portal");
                    lamco_clipboard_core::image::dib_to_jpeg(&data).map_err(|e| {
                        error!("DIB to JPEG conversion failed: {}", e);
                        ClipboardError::Core(e)
                    })?
                }
            } else {
                error!(
                    "Image data too small for bitmap header: {} bytes",
                    data.len()
                );
                return Err(ClipboardError::Core(
                    lamco_clipboard_core::ClipboardError::ImageDecode(
                        "Data too small for bitmap".to_string(),
                    ),
                ));
            }
        } else if requested_mime.starts_with("image/bmp")
            || requested_mime.starts_with("image/x-bmp")
        {
            // Portal wants BMP, Windows sent DIB
            trace!(" Converting DIB to BMP for Portal");
            lamco_clipboard_core::image::dib_to_bmp(&data).map_err(|e| {
                error!("DIB to BMP conversion failed: {}", e);
                ClipboardError::Core(e)
            })?
        } else if requested_mime == "text/rtf" || requested_mime == "application/rtf" {
            // RTF is plain ASCII/Latin-1 text, NOT UTF-16
            // Windows CF_RTF sends raw RTF markup as bytes
            debug!(
                "RTF format detected ({} bytes) - passing through with line ending conversion",
                data.len()
            );

            // Convert to string (lossy for any invalid UTF-8, though RTF should be ASCII)
            let text = String::from_utf8_lossy(&data);

            // Sanitize for Linux: CRLF → LF, remove null bytes
            let sanitized = sanitize_text_for_linux(&text);
            let rtf_bytes = sanitized.as_bytes().to_vec();

            debug!(
                "RTF: {} raw bytes → {} bytes after line ending conversion",
                data.len(),
                rtf_bytes.len()
            );
            if !rtf_bytes.is_empty() {
                let preview_len = rtf_bytes.len().min(80);
                debug!(
                    "RTF preview: {:?}",
                    String::from_utf8_lossy(&rtf_bytes[..preview_len])
                );
            }
            rtf_bytes
        } else if (requested_mime.starts_with("text/plain")
            || requested_mime.starts_with("text/html"))
            && data.len() >= 2
        {
            // text/plain and text/html from Windows are UTF-16LE (CF_UNICODETEXT)
            // MIME may have charset suffix like "text/plain;charset=utf-8"
            // Convert UTF-16LE to UTF-8 with line ending conversion
            let utf16_data: Vec<u16> = data
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .take_while(|&c| c != 0) // Stop at null terminator
                .collect();

            // Use lossy conversion to handle malformed UTF-16
            // This handles invalid surrogates and replaces them with U+FFFD
            let text = String::from_utf16_lossy(&utf16_data);

            // Sanitize for Linux: CRLF → LF, remove null bytes
            let sanitized = sanitize_text_for_linux(&text);
            let utf8_bytes = sanitized.as_bytes().to_vec();

            debug!(
                "Converted UTF-16 to UTF-8: {} UTF-16 chars ({} bytes) → {} UTF-8 bytes with LF line endings",
                utf16_data.len(),
                data.len(),
                utf8_bytes.len()
            );
            if !sanitized.is_empty() {
                debug!("Text preview: {:?}", &sanitized[..sanitized.len().min(50)]);
            }
            utf8_bytes
        } else {
            // Unknown format or too small - pass through
            debug!(
                "Unknown format or small data, using raw {} bytes",
                data.len()
            );
            data
        };

        // Deliver converted data via clipboard provider
        let provider = provider_opt.as_ref().expect("provider checked above");

        match provider
            .complete_transfer(serial, &requested_mime, portal_data.clone(), true)
            .await
        {
            Ok(()) => {
                info!(
                    "Clipboard data delivered via {} provider (serial {})",
                    provider.name(),
                    serial
                );

                // Cache the delivered data so subsequent SelectionTransfer requests
                // for the same MIME type can be served without re-fetching from RDP client
                transfer_data_cache
                    .write()
                    .await
                    .insert(requested_mime.clone(), portal_data);

                // Cancel unfulfilled requests (apps send multiple MIME requests per paste)
                let mut pending = pending_portal_requests.write().await;
                let unfulfilled: Vec<(u32, String)> = pending
                    .iter()
                    .filter(|(s, _, _)| *s != serial)
                    .map(|(s, m, _)| (*s, m.clone()))
                    .collect();
                pending.clear();
                drop(pending);

                for (unfulfilled_serial, mime) in &unfulfilled {
                    if let Err(e) = provider
                        .complete_transfer(*unfulfilled_serial, mime, vec![], false)
                        .await
                    {
                        warn!("Failed to cancel serial {}: {}", unfulfilled_serial, e);
                    }
                }
            }
            Err(e) => {
                error!("Failed to deliver clipboard data via provider: {:#}", e);
                pending_portal_requests
                    .write()
                    .await
                    .retain(|(s, _, _)| *s != serial);
            }
        }

        Ok(())
    }

    /// Provider-based data response handler
    ///
    /// Called when clipboard_provider is set but legacy Portal is unavailable.
    /// Mirrors the logic of handle_rdp_data_response but uses the provider's
    /// complete_transfer() API instead of Portal's write_selection_data/selection_write_done.
    /// Handle RDP data error (must notify clipboard provider to prevent retry crash)
    ///
    /// This is called when the RDP client responds with FormatDataResponse(error=true),
    /// which is normal protocol behavior when the client doesn't have the requested format.
    /// Per MS-RDPECLIP, this is expected and not an error condition.
    pub(super) async fn handle_rdp_data_error(
        clipboard_provider: &SharedClipboardProvider,
        pending_portal_requests: &PendingPortalRequests,
    ) -> Result<()> {
        debug!("RDP FormatDataResponse: format not available, notifying clipboard backend");

        let pending = pending_portal_requests.read().await;
        let entries: Vec<(u32, String)> = pending.iter().map(|(s, m, _)| (*s, m.clone())).collect();
        drop(pending);

        match *clipboard_provider.read().await {
            Some(ref provider) => {
                for (serial, mime_type) in &entries {
                    debug!(
                        "Notifying {} provider of transfer failure (serial {})",
                        provider.name(),
                        serial
                    );
                    if let Err(e) = provider
                        .complete_transfer(*serial, mime_type, vec![], false)
                        .await
                    {
                        warn!("Failed to notify provider of transfer failure: {:#}", e);
                    }
                }
            }
            _ => {
                warn!("No clipboard provider available to notify of transfer failure");
            }
        }

        pending_portal_requests.write().await.clear();
        Ok(())
    }
}
