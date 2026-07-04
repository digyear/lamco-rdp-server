//! Unified Server Connection
//!
//! Provides a unified interface for managing the server, whether via:
//! - D-Bus connection to an already-running service (preferred)
//! - Spawned child process (we own the lifecycle)
//! - External PID discovered via PID file (signal-based control only)
//!
//! The GUI tries D-Bus first, then PID file, then spawn.

use std::time::Duration;

use tokio::sync::mpsc;

use super::{
    dbus_client::DbusClient,
    server_process::{ServerLogLine, ServerProcess},
};
use crate::config::Config;

/// Connection mode for the server
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionMode {
    /// Connected via D-Bus to an existing service
    DBus,
    /// Managing a spawned child process
    Process,
    /// Discovered an externally-managed server via PID file (signal-based control)
    External,
    /// Not connected
    Disconnected,
}

/// Externally-discovered server (from PID file). Signal-based control only —
/// no IPC, so uptime is approximate and connection count is unavailable.
pub struct ExternalServer {
    pid: u32,
    address: String,
    discovered_at: std::time::Instant,
}

impl ExternalServer {
    pub fn new(pid: u32, address: String) -> Self {
        Self {
            pid,
            address,
            discovered_at: std::time::Instant::now(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Liveness via `kill(pid, 0)` — does not deliver a signal, just probes
    /// whether the kernel can route one. EPERM means alive-but-not-ours.
    pub fn is_running(&self) -> bool {
        use nix::{sys::signal::kill, unistd::Pid};
        matches!(
            kill(Pid::from_raw(self.pid as i32), None),
            Ok(()) | Err(nix::errno::Errno::EPERM)
        )
    }

    /// Approximate uptime — the time since the GUI discovered the PID file.
    /// The real start time lives in `/proc/<pid>/stat` field 22; the parsing
    /// adds complexity for a status field that's already imprecise.
    pub fn uptime(&self) -> Duration {
        self.discovered_at.elapsed()
    }

    /// Stop the externally-managed server (SIGTERM, wait, escalate to SIGKILL).
    pub fn stop(&self) -> Result<(), String> {
        terminate_server_pid(self.pid)
    }
}

/// Read the running server's PID from the canonical PID file
/// (`$XDG_RUNTIME_DIR/lamco-rdp-server.pid`), which the server writes at startup.
fn read_server_pid_file() -> Option<u32> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let path = std::path::PathBuf::from(runtime_dir).join("lamco-rdp-server.pid");
    std::fs::read_to_string(&path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Guard against a stale/recycled PID: only signal a process whose `comm` is our
/// server. Linux truncates `comm` to 15 bytes, so `lamco-rdp-server` reads back
/// as `lamco-rdp-serve`.
fn pid_is_lamco_server(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|c| c.trim() == "lamco-rdp-serve")
}

/// Terminate a server by PID: confirm it is ours and alive, send SIGTERM (the
/// server's handler does a graceful shutdown), wait up to 5s, then SIGKILL.
/// The headless server's SIGTERM handler does graceful shutdown (Quit event +
/// broadcast), so the grace window is real.
fn terminate_server_pid(pid_raw: u32) -> Result<(), String> {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };

    let pid = Pid::from_raw(pid_raw as i32);
    let alive = || matches!(kill(pid, None), Ok(()) | Err(nix::errno::Errno::EPERM));

    if !alive() {
        return Ok(());
    }
    if !pid_is_lamco_server(pid_raw) {
        tracing::warn!("PID {pid_raw} is not a lamco-rdp-server process; refusing to signal it");
        return Ok(());
    }

    kill(pid, Signal::SIGTERM).map_err(|e| format!("SIGTERM to PID {pid_raw}: {e}"))?;
    tracing::info!("SIGTERM sent to server PID {pid_raw}");

    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if !alive() {
            return Ok(());
        }
    }

    tracing::warn!("PID {pid_raw} did not exit within 5s of SIGTERM; sending SIGKILL");
    kill(pid, Signal::SIGKILL).map_err(|e| format!("SIGKILL to PID {pid_raw}: {e}"))?;
    Ok(())
}

/// Unified server connection
///
/// Abstracts whether we're talking to a D-Bus service, a spawned process, or
/// an externally-discovered server (PID file).
pub enum ServerConnection {
    /// Connected via D-Bus
    DBus(DbusClient),

    /// Managing a child process
    Process(ServerProcess),

    /// Externally-managed server discovered via PID file
    External(ExternalServer),
}

impl ServerConnection {
    /// Try to connect to an existing D-Bus server
    ///
    /// Returns None if no server is available.
    pub async fn try_dbus() -> Option<Self> {
        DbusClient::try_connect().await.map(Self::DBus)
    }

    /// Wrap an externally-discovered server (from PID file).
    pub fn external(pid: u32, address: String) -> Self {
        Self::External(ExternalServer::new(pid, address))
    }

    /// Start a new server process
    ///
    /// Falls back to spawning if D-Bus connection is not available.
    pub fn spawn_process(
        config: &Config,
        log_sender: mpsc::UnboundedSender<ServerLogLine>,
    ) -> Result<Self, String> {
        ServerProcess::start(config, log_sender)
            .map(Self::Process)
            .map_err(|e| e.to_string())
    }

    /// Connect to server - tries D-Bus first, then spawns if needed
    ///
    /// This is the recommended method for connecting to the server.
    pub async fn connect(
        config: &Config,
        log_sender: mpsc::UnboundedSender<ServerLogLine>,
        prefer_dbus: bool,
    ) -> Result<Self, String> {
        // Try D-Bus first if preferred
        if prefer_dbus {
            if let Some(mut client) = DbusClient::try_connect().await {
                client.set_log_sender(log_sender);
                tracing::info!("Connected to server via D-Bus");
                return Ok(Self::DBus(client));
            }
            tracing::debug!("D-Bus server not available, falling back to process spawn");
        }

        // Fall back to spawning a process
        Self::spawn_process(config, log_sender)
    }

    pub fn mode(&self) -> ConnectionMode {
        match self {
            Self::DBus(_) => ConnectionMode::DBus,
            Self::Process(_) => ConnectionMode::Process,
            Self::External(_) => ConnectionMode::External,
        }
    }

    pub async fn is_running(&self) -> bool {
        match self {
            Self::DBus(client) => client.is_running().await,
            Self::Process(process) => process.is_running(),
            Self::External(ext) => ext.is_running(),
        }
    }

    pub async fn address(&self) -> String {
        match self {
            Self::DBus(client) => client.address().await.unwrap_or_default(),
            Self::Process(process) => process.address().to_string(),
            Self::External(ext) => ext.address().to_string(),
        }
    }

    pub async fn uptime(&self) -> Duration {
        match self {
            Self::DBus(client) => client.uptime().await.unwrap_or_default(),
            Self::Process(process) => process.uptime(),
            Self::External(ext) => ext.uptime(),
        }
    }

    pub async fn active_connections(&self) -> u32 {
        match self {
            Self::DBus(client) => client.active_connections().await.unwrap_or(0),
            Self::Process(_) | Self::External(_) => 0,
        }
    }

    /// Stop the server (explicit "Stop Server" action — not the window-close path).
    ///
    /// - D-Bus: terminates the server by the PID in its PID file (SIGTERM, then
    ///   SIGKILL). The GUI is the server's manager, so "Stop" means stop.
    /// - Process: terminates the child we spawned.
    /// - External: sends SIGTERM, waits ~5s, escalates to SIGKILL if needed.
    ///   The headless server's own SIGTERM handler does graceful shutdown.
    pub fn stop(&mut self) -> Result<(), String> {
        match self {
            Self::DBus(_) => {
                // The GUI connected to an already-running `--dbus-service` server. The
                // explicit "Stop Server" button must actually stop it — the prior
                // behaviour only disconnected the GUI, leaving the server running and
                // holding the port (and the GUI showing "Stopped"). The D-Bus interface
                // exposes no quit method, so terminate by the PID the server wrote to its
                // PID file. This is the explicit-stop path only; window-close / detach
                // (`close_stops_server`) is handled separately and is unaffected.
                match read_server_pid_file() {
                    Some(pid) => terminate_server_pid(pid),
                    None => {
                        tracing::warn!(
                            "D-Bus stop: no PID file found; disconnecting only (server may keep running)"
                        );
                        Ok(())
                    }
                }
            }
            Self::Process(process) => process.stop().map_err(|e| e.to_string()),
            Self::External(ext) => ext.stop(),
        }
    }

    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::DBus(_) => None,
            Self::Process(process) => Some(process.pid()),
            Self::External(ext) => Some(ext.pid()),
        }
    }

    pub fn as_dbus(&self) -> Option<&DbusClient> {
        match self {
            Self::DBus(client) => Some(client),
            Self::Process(_) | Self::External(_) => None,
        }
    }

    pub fn as_dbus_mut(&mut self) -> Option<&mut DbusClient> {
        match self {
            Self::DBus(client) => Some(client),
            Self::Process(_) | Self::External(_) => None,
        }
    }

    /// Detach the server so it keeps running when GUI exits.
    ///
    /// D-Bus and External are already detached (we don't own their lifecycle).
    /// Process needs an explicit flag to skip the kill-on-drop path.
    pub fn detach(&mut self) {
        if let Self::Process(process) = self {
            process.detach();
        }
    }
}

impl std::fmt::Debug for ServerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DBus(_) => write!(f, "ServerConnection::DBus"),
            Self::Process(p) => write!(f, "ServerConnection::Process(pid={})", p.pid()),
            Self::External(e) => write!(f, "ServerConnection::External(pid={})", e.pid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_mode() {
        // Can't easily test without a running server
        // Just verify the enum works
        assert_eq!(ConnectionMode::DBus, ConnectionMode::DBus);
        assert_ne!(ConnectionMode::DBus, ConnectionMode::Process);
        assert_ne!(ConnectionMode::External, ConnectionMode::Process);
    }

    #[test]
    fn external_server_kill_self_unreachable() {
        // A PID we know isn't running: PID 1 owned by root, but we test
        // is_running via signal-0 probe — this should succeed (PID 1 always
        // exists). Use it as a smoke test that the probe code links.
        let ext = ExternalServer::new(1, "0.0.0.0:3389".to_string());
        // PID 1 is always alive on a running system.
        assert!(ext.is_running());
        // Don't call stop() — would either no-op (EPERM, expected) or kill init.
    }
}
