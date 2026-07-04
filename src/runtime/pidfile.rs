//! PID file management for lamco-rdp-server.
//!
//! Single canonical path per user: `$XDG_RUNTIME_DIR/lamco-rdp-server.pid`.
//! The server writes it at startup so any launcher — GUI, systemd, SSH+nohup —
//! produces an authoritative "lamco-rdp-server is alive at PID X" signal that
//! the GUI can read for detection. Without this file the GUI shows a "Start
//! Server" button while a server is already running.
//!
//! Scope is intentionally narrow:
//!
//! - This module is for the lamco-rdp-server binary only. lamco-qemu-rdp is
//!   a separate product with separate consumers (Proxmox tooling) and gets
//!   its own mechanism if and when one is needed.
//! - One instance per user is the only supported case. Multi-instance
//!   lamco-rdp-server is not a current requirement; if it ever becomes one,
//!   add an `--instance NAME` CLI flag that swaps the filename — not here.
//! - Mixed environments are not a concern: qemu-rdp lives in `/run/lamco/`,
//!   not `$XDG_RUNTIME_DIR`; different filename either way. The GUI never
//!   looks at qemu-rdp paths.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const FILENAME: &str = "lamco-rdp-server.pid";

/// Canonical PID file path for the running user's lamco-rdp-server.
///
/// `$XDG_RUNTIME_DIR/lamco-rdp-server.pid` when the env var is set (always
/// the case inside a user session), otherwise `/tmp/lamco-rdp-server.pid`.
/// Matches the path the GUI's `src/gui/server_process.rs::pid_file_path` has
/// expected since the GUI was written.
pub fn path() -> PathBuf {
    path_with_base(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

/// Path resolution split out so tests can exercise the base-directory logic
/// without mutating process-global environment state.
fn path_with_base(xdg_runtime_dir: Option<&str>) -> PathBuf {
    let base = xdg_runtime_dir
        .filter(|s| !s.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    base.join(FILENAME)
}

/// Write the current process PID to the canonical file.
pub fn write(path: &Path) -> Result<()> {
    let pid = std::process::id();
    std::fs::write(path, pid.to_string())
        .with_context(|| format!("writing PID file: {}", path.display()))?;
    Ok(())
}

/// Remove the PID file. Best-effort cleanup — errors are logged, not returned.
pub fn remove(path: &Path) {
    if path.exists()
        && let Err(e) = std::fs::remove_file(path)
    {
        tracing::debug!("Failed to remove PID file {}: {}", path.display(), e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_uses_xdg_runtime_dir_when_set() {
        assert_eq!(
            path_with_base(Some("/tmp/test-runtime-pidfile-1")),
            PathBuf::from("/tmp/test-runtime-pidfile-1/lamco-rdp-server.pid"),
        );
    }

    #[test]
    fn path_falls_back_to_tmp_when_xdg_empty() {
        assert_eq!(
            path_with_base(Some("")),
            PathBuf::from("/tmp/lamco-rdp-server.pid"),
        );
    }

    #[test]
    fn path_falls_back_to_tmp_when_xdg_unset() {
        assert_eq!(
            path_with_base(None),
            PathBuf::from("/tmp/lamco-rdp-server.pid")
        );
    }

    #[test]
    fn write_then_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lamco-rdp-server.pid");
        write(&p).unwrap();
        assert!(p.exists());
        let content = std::fs::read_to_string(&p).unwrap();
        assert_eq!(content, std::process::id().to_string());
        remove(&p);
        assert!(!p.exists());
    }
}
