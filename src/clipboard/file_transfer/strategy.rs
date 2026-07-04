//! File Transfer Mode Selection
//!
//! Determines how clipboard files from Windows are materialized on the
//! Linux filesystem. Follows the same pattern as
//! [`ClipboardIntegrationMode`](super::super::strategy::ClipboardIntegrationMode)
//! for clipboard text/image strategies.

use tracing::info;

/// File transfer materialization mode.
///
/// Determines how files from the RDP clipboard are made available to Linux
/// file managers. Selected based on deployment mode, FUSE availability,
/// and user configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransferMode {
    /// FUSE on-demand virtual filesystem (native only).
    ///
    /// Files appear instantly as virtual entries. Data is fetched from
    /// Windows via RDP only when a Linux application reads the file.
    /// Zero disk cost for unread files. Requires `/dev/fuse` and
    /// `fusermount3`. Not available in Flatpak sandboxes.
    Fuse,

    /// Staging via eager download to temp directory (universal).
    ///
    /// All files are downloaded from Windows immediately when the
    /// clipboard format is announced. Files are stored in the configured
    /// download directory (default: `~/Downloads`). Works everywhere
    /// including Flatpak. Higher latency and disk cost for large file
    /// lists where only some files are pasted.
    Staging,

    /// Portal FileTransfer (future, not yet implemented).
    ///
    /// Uses `org.freedesktop.portal.FileTransfer` D-Bus API to hand
    /// files to the system through the document portal. Would work in
    /// Flatpak sandboxes without `/dev/fuse`. Currently no file manager
    /// consumes the `application/vnd.portal.filetransfer` MIME type for
    /// paste operations, so this is reserved for future use.
    Portal,
}

impl FileTransferMode {
    /// Select file transfer mode based on environment and config.
    ///
    /// Priority:
    /// 1. User config override (`file_transfer_mode` setting)
    /// 2. Auto-detection based on Flatpak status
    pub fn select(in_flatpak: bool, config_override: Option<&str>) -> Self {
        info!("  ─────────────────────────────────────────────────");
        info!("  File Transfer Mode Selection");

        if let Some(override_str) = config_override {
            if let Some(mode) = Self::from_override_string(override_str) {
                info!("  Mode: MANUAL OVERRIDE → {:?}", mode);
                if in_flatpak && mode == Self::Fuse {
                    info!(
                        "  ⚠  FUSE requested in Flatpak — will fail at init, expect staging fallback"
                    );
                }
                info!("  ─────────────────────────────────────────────────");
                return mode;
            }
            tracing::warn!(
                "  Invalid file_transfer_mode '{}', using auto",
                override_str
            );
        }

        let mode = Self::auto_select(in_flatpak);
        info!("  Mode: auto → {:?}", mode);
        info!("  ─────────────────────────────────────────────────");
        mode
    }

    fn auto_select(in_flatpak: bool) -> Self {
        if in_flatpak {
            // Flatpak sandbox has no /dev/fuse access.
            // Portal FileTransfer is not viable yet (no file manager consumes it).
            Self::Staging
        } else {
            // Native: prefer FUSE for on-demand fetch.
            // Falls back to staging at runtime if FUSE init fails.
            Self::Fuse
        }
    }

    fn from_override_string(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" => None, // Let auto_select decide
            "fuse" => Some(Self::Fuse),
            "staging" => Some(Self::Staging),
            "portal" => Some(Self::Portal),
            _ => None,
        }
    }

    /// Human-readable mode name for logging and GUI.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fuse => "FUSE On-Demand",
            Self::Staging => "Staging Download",
            Self::Portal => "Portal FileTransfer",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_select_native_prefers_fuse() {
        let mode = FileTransferMode::select(false, None);
        assert_eq!(mode, FileTransferMode::Fuse);
    }

    #[test]
    fn test_auto_select_flatpak_uses_staging() {
        let mode = FileTransferMode::select(true, None);
        assert_eq!(mode, FileTransferMode::Staging);
    }

    #[test]
    fn test_override_fuse() {
        let mode = FileTransferMode::select(false, Some("fuse"));
        assert_eq!(mode, FileTransferMode::Fuse);
    }

    #[test]
    fn test_override_staging() {
        let mode = FileTransferMode::select(false, Some("staging"));
        assert_eq!(mode, FileTransferMode::Staging);
    }

    #[test]
    fn test_override_portal() {
        let mode = FileTransferMode::select(false, Some("portal"));
        assert_eq!(mode, FileTransferMode::Portal);
    }

    #[test]
    fn test_override_case_insensitive() {
        let mode = FileTransferMode::select(false, Some("STAGING"));
        assert_eq!(mode, FileTransferMode::Staging);
    }

    #[test]
    fn test_invalid_override_falls_back_to_auto() {
        // Invalid override in native mode → auto → Fuse
        let mode = FileTransferMode::select(false, Some("invalid"));
        assert_eq!(mode, FileTransferMode::Fuse);

        // Invalid override in Flatpak → auto → Staging
        let mode = FileTransferMode::select(true, Some("nonsense"));
        assert_eq!(mode, FileTransferMode::Staging);
    }

    #[test]
    fn test_auto_override_returns_auto() {
        // "auto" override = same as no override
        let mode = FileTransferMode::select(false, Some("auto"));
        assert_eq!(mode, FileTransferMode::Fuse);
    }

    #[test]
    fn test_mode_names() {
        assert_eq!(FileTransferMode::Fuse.name(), "FUSE On-Demand");
        assert_eq!(FileTransferMode::Staging.name(), "Staging Download");
        assert_eq!(FileTransferMode::Portal.name(), "Portal FileTransfer");
    }
}
