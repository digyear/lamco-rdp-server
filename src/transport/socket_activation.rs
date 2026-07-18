//! systemd socket-activation multi-fd intake.
//!
//! Parses the `LISTEN_FDS`, `LISTEN_FDNAMES`, `LISTEN_PID` environment
//! variables systemd sets when launching a socket-activated service, then
//! hands each passed file descriptor back as an `OwnedFd` indexed by name.
//!
//! Phase 4 of the transport-layer refactor. Generalizes the single-fd path
//! Phase 2 shipped (`qemu_main.rs::detect_socket_activation`) to N labeled
//! fds so a per-VM `.socket` template can declare TCP + Unix + vsock + WSS
//! `ListenStream=` lines together and route each to the appropriate listener
//! inside the binary.
//!
//! See `docs/design/transport/TRANSPORT-PHASE-4-SDS-2026-05-17.md` and
//! `docs/analysis/TRANSPORT-PHASE-4-ARCHITECTURE-VALIDATION-2026-05-17.md`.

use std::{collections::HashMap, os::fd::OwnedFd};

use thiserror::Error;
use tracing::info;

/// First fd systemd guarantees to pass via socket activation. Per `sd_listen_fds(3)`.
const SD_LISTEN_FDS_START: i32 = 3;

/// Map of name → `OwnedFd` for sockets handed to us by systemd.
///
/// Empty when `LISTEN_FDS` is unset, zero, or rejected (PID mismatch).
/// Names come from `LISTEN_FDNAMES` (`:`-separated, set by systemd from
/// `FileDescriptorName=` in the `.socket` unit). When `LISTEN_FDNAMES` is
/// absent — older systemd or unnamed-fd configurations — fds get
/// positional names `unknown.0`, `unknown.1`, …, and `take_first_unknown`
/// is available for the Phase 2 single-fd flow's backward compat.
///
/// `OwnedFd` enforces single-consumer transfer: once a name is taken via
/// `take()`, it cannot be taken again.
#[derive(Debug, Default)]
pub struct ActivatedFds {
    fds: HashMap<String, OwnedFd>,
}

#[derive(Debug, Error)]
pub enum ActivationError {
    #[error("LISTEN_FDS={raw:?}: {reason}")]
    Parse { raw: String, reason: String },
    #[error("LISTEN_FDS={fds} but LISTEN_FDNAMES has {names} entries — mismatch")]
    LengthMismatch { fds: usize, names: usize },
    #[error(
        "LISTEN_PID={listen_pid} does not match getpid()={current_pid} — refusing to consume inherited fds"
    )]
    PidMismatch { listen_pid: i32, current_pid: i32 },
    #[error("duplicate fd name {0:?} in LISTEN_FDNAMES")]
    DuplicateName(String),
}

impl ActivatedFds {
    /// Parse `LISTEN_FDS`, `LISTEN_FDNAMES`, `LISTEN_PID` from the environment
    /// and consume them (env vars are unset so a re-invocation returns empty).
    ///
    /// Returns an empty `ActivatedFds` when `LISTEN_FDS` is unset or zero —
    /// the binary should fall back to programmatic `bind()`.
    ///
    /// # Safety
    ///
    /// Wraps fds starting at `SD_LISTEN_FDS_START` (3) into `OwnedFd` values
    /// via `OwnedFd::from_raw_fd`. The systemd protocol guarantees these fds
    /// are valid, open listening sockets — we trust the guarantee.
    ///
    /// Must be called at most once early in `main()`. Subsequent calls see
    /// the env vars unset and return an empty container.
    #[expect(
        unsafe_code,
        reason = "systemd socket activation requires from_raw_fd on the LISTEN_FDS-passed fds; safety justified inline"
    )]
    pub fn from_env() -> Result<Self, ActivationError> {
        let listen_fds_raw = std::env::var("LISTEN_FDS").ok();
        let listen_fdnames_raw = std::env::var("LISTEN_FDNAMES").ok();
        let listen_pid_raw = std::env::var("LISTEN_PID").ok();

        // Always consume the env vars regardless of outcome — they're meant for
        // exactly one process invocation and leaving them set would confuse
        // any re-exec or child process.
        clear_listen_env();

        let Some(raw_fds) = listen_fds_raw else {
            return Ok(Self {
                fds: HashMap::new(),
            });
        };

        let fd_count: usize = raw_fds.parse().map_err(|e| ActivationError::Parse {
            raw: raw_fds.clone(),
            reason: format!("not a non-negative integer: {e}"),
        })?;

        if fd_count == 0 {
            return Ok(Self {
                fds: HashMap::new(),
            });
        }

        // PID check: systemd sets LISTEN_PID to the target process's pid. If
        // an env-leak from a parent process gave us fds 3..N that don't
        // actually belong to us, refuse to consume them.
        if let Some(raw_pid) = listen_pid_raw {
            let listen_pid: i32 = raw_pid.parse().map_err(|e| ActivationError::Parse {
                raw: raw_pid.clone(),
                reason: format!("LISTEN_PID not an integer: {e}"),
            })?;
            #[expect(
                clippy::cast_possible_wrap,
                reason = "process IDs fit in i32 on all platforms we target"
            )]
            let current_pid = std::process::id() as i32;
            if listen_pid != current_pid {
                return Err(ActivationError::PidMismatch {
                    listen_pid,
                    current_pid,
                });
            }
        }

        // Resolve names — either from LISTEN_FDNAMES (post-systemd-227) or
        // synthesize positional unknown.N labels.
        let names: Vec<String> = match listen_fdnames_raw {
            Some(raw) => raw.split(':').map(str::to_string).collect(),
            None => (0..fd_count).map(|i| format!("unknown.{i}")).collect(),
        };

        if names.len() != fd_count {
            return Err(ActivationError::LengthMismatch {
                fds: fd_count,
                names: names.len(),
            });
        }

        // Check for duplicate names BEFORE wrapping fds (no partial-consume
        // on error).
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            if !seen.insert(name.as_str()) {
                return Err(ActivationError::DuplicateName(name.clone()));
            }
        }

        // Wrap each fd. SAFETY discussion is at the function-level comment;
        // additionally: we only reach this point after the PID check has
        // confirmed the fds belong to this process.
        let mut fds = HashMap::with_capacity(fd_count);
        for (i, name) in names.into_iter().enumerate() {
            #[expect(
                clippy::cast_possible_wrap,
                reason = "SD_LISTEN_FDS_START + i is always within i32 range for any realistic fd count"
            )]
            let raw_fd = SD_LISTEN_FDS_START + i as i32;
            // SAFETY: systemd guarantees fds [3 .. 3+fd_count) are open
            // listening sockets belonging to this process (verified via the
            // PID check above). Wrapping into `OwnedFd` takes exclusive
            // ownership; systemd does NOT close them itself.
            let owned = unsafe {
                use std::os::fd::FromRawFd;
                OwnedFd::from_raw_fd(raw_fd)
            };
            fds.insert(name, owned);
        }

        info!(
            count = fd_count,
            names = ?fds.keys().collect::<Vec<_>>(),
            "Socket activation: consumed fds from systemd"
        );

        Ok(Self { fds })
    }

    /// Take the fd registered under `name`, transferring ownership.
    ///
    /// Returns `None` if no fd was passed under that name (or it has already
    /// been taken).
    pub fn take(&mut self, name: &str) -> Option<OwnedFd> {
        self.fds.remove(name)
    }

    /// Take any one fd regardless of name. Used by the Phase 2 backward-compat
    /// path: an older `.socket` unit without `FileDescriptorName=` directives
    /// passes a single unnamed fd which gets the synthesized `unknown.0`
    /// label; the caller doesn't know that label and just wants "the fd".
    ///
    /// Returns `None` when no fds remain.
    pub fn take_first_unknown(&mut self) -> Option<OwnedFd> {
        // Find any name starting with `unknown.` — these are positional
        // synthesized labels we know the operator didn't intend as
        // semantic names.
        let key = self
            .fds
            .keys()
            .find(|k| k.starts_with("unknown."))
            .cloned()?;
        self.fds.remove(&key)
    }

    /// Take any one fd regardless of its systemd-provided name.
    ///
    /// This supports older single-socket desktop units where systemd uses the
    /// socket unit name when `FileDescriptorName=` was not configured.
    pub fn take_first(&mut self) -> Option<OwnedFd> {
        let key = self.fds.keys().next().cloned()?;
        self.fds.remove(&key)
    }

    /// True if no fds remain (either none arrived or all have been taken).
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }

    /// Count of fds currently held.
    pub fn len(&self) -> usize {
        self.fds.len()
    }

    /// Names of fds not yet consumed. Used by deployment code to warn on
    /// misconfigured `.socket` units that pass fds we don't recognize.
    pub fn remaining_names(&self) -> Vec<&str> {
        self.fds.keys().map(String::as_str).collect()
    }

    /// True if any passed socket is an IP listener bound to a non-loopback
    /// (routable) address.
    ///
    /// The startup exposure guard uses this to catch a stale
    /// `ListenStream=0.0.0.0:…` in a `.socket` unit — a routable bind the
    /// programmatic-bind check would miss. AF_UNIX and AF_VSOCK fds report no
    /// IP socket address and are treated as non-routable (the relay terminates
    /// auth before reaching them). An fd whose address can't be read fails
    /// closed (counted routable) — it cannot be proven safe, though in practice
    /// `getsockname` on a systemd-passed listening socket succeeds. Borrows the
    /// fds without consuming them.
    pub fn has_routable_inet_listener(&self) -> bool {
        self.fds
            .values()
            .any(|fd| match socket2::SockRef::from(fd).local_addr() {
                Ok(addr) => addr
                    .as_socket()
                    .is_some_and(|sock| !sock.ip().is_loopback()),
                Err(_) => true,
            })
    }

    /// True if a passed fd will become the TCP listener — a `tcp` fd or a
    /// synthesized `unknown.*` fd. When this is false the deployment binds
    /// `listen_addr` programmatically instead, so the exposure guard must
    /// check `listen_addr`'s routability. Mirrors the
    /// `take("tcp").or_else(take_first_unknown())` selection in
    /// `QemuDeployment::build_listeners` — keep the two in sync.
    pub fn provides_tcp_listener(&self) -> bool {
        self.fds.contains_key("tcp") || self.fds.keys().any(|k| k.starts_with("unknown."))
    }
}

/// Clear the LISTEN_* env vars so they don't leak into child processes or
/// re-exec invocations.
#[expect(
    unsafe_code,
    reason = "std::env::remove_var is unsafe in Rust 2024; called once from main() before any thread spawn that might read environ"
)]
fn clear_listen_env() {
    // SAFETY: `set_var` / `remove_var` are unsafe as of Rust 2024 edition
    // because they're not thread-safe; only the calling thread sees the
    // change in environ. We call this exactly once from `main()` before any
    // threads spawn that might read the environment.
    unsafe {
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
        std::env::remove_var("LISTEN_PID");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-mutation isn't thread-safe; serialize tests that touch LISTEN_*.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Helper: set the LISTEN_* env vars to a known state for a test.
    #[expect(
        unsafe_code,
        reason = "test helper mutates env behind ENV_MUTEX serialization; isolated from production"
    )]
    fn set_env(fds: Option<&str>, names: Option<&str>, pid: Option<&str>) {
        // SAFETY: see clear_listen_env(); tests hold ENV_MUTEX so no other
        // test thread can be reading environ concurrently. Production code
        // doesn't call set_var.
        unsafe {
            for (key, val) in [
                ("LISTEN_FDS", fds),
                ("LISTEN_FDNAMES", names),
                ("LISTEN_PID", pid),
            ] {
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Helper: assert all LISTEN_* env vars are unset.
    fn assert_env_cleared() {
        assert!(std::env::var("LISTEN_FDS").is_err(), "LISTEN_FDS still set");
        assert!(
            std::env::var("LISTEN_FDNAMES").is_err(),
            "LISTEN_FDNAMES still set"
        );
        assert!(std::env::var("LISTEN_PID").is_err(), "LISTEN_PID still set");
    }

    #[test]
    fn from_env_returns_empty_when_listen_fds_unset() {
        let _g = ENV_MUTEX.lock().unwrap();
        set_env(None, None, None);

        let fds = ActivatedFds::from_env().unwrap();
        assert!(fds.is_empty());
        assert_eq!(fds.len(), 0);
        assert_env_cleared();
    }

    #[test]
    fn from_env_returns_empty_when_listen_fds_zero() {
        let _g = ENV_MUTEX.lock().unwrap();
        set_env(Some("0"), None, None);

        let fds = ActivatedFds::from_env().unwrap();
        assert!(fds.is_empty());
        assert_env_cleared();
    }

    #[test]
    fn from_env_rejects_non_numeric_listen_fds() {
        let _g = ENV_MUTEX.lock().unwrap();
        set_env(Some("abc"), None, None);

        let err = ActivatedFds::from_env().unwrap_err();
        match err {
            ActivationError::Parse { .. } => {}
            other => panic!("expected Parse, got {other:?}"),
        }
        assert_env_cleared();
    }

    #[test]
    fn from_env_rejects_length_mismatch() {
        let _g = ENV_MUTEX.lock().unwrap();
        // Use this process's pid so PID check passes; we want to fail on
        // length-mismatch specifically.
        let pid = std::process::id().to_string();
        set_env(Some("2"), Some("tcp"), Some(&pid));

        let err = ActivatedFds::from_env().unwrap_err();
        match err {
            ActivationError::LengthMismatch { fds: 2, names: 1 } => {}
            other => panic!("expected LengthMismatch{{2,1}}, got {other:?}"),
        }
        assert_env_cleared();
    }

    #[test]
    fn from_env_rejects_duplicate_names() {
        let _g = ENV_MUTEX.lock().unwrap();
        let pid = std::process::id().to_string();
        set_env(Some("2"), Some("tcp:tcp"), Some(&pid));

        let err = ActivatedFds::from_env().unwrap_err();
        match err {
            ActivationError::DuplicateName(name) if name == "tcp" => {}
            other => panic!("expected DuplicateName(\"tcp\"), got {other:?}"),
        }
        assert_env_cleared();
    }

    #[test]
    fn from_env_rejects_pid_mismatch() {
        let _g = ENV_MUTEX.lock().unwrap();
        // Set LISTEN_PID to a pid that's almost certainly not us.
        // PID 1 (init) is the safest sentinel: it's stable and we're not it.
        set_env(Some("1"), Some("tcp"), Some("1"));

        let err = ActivatedFds::from_env().unwrap_err();
        match err {
            ActivationError::PidMismatch { listen_pid: 1, .. } => {}
            other => panic!("expected PidMismatch{{1}}, got {other:?}"),
        }
        assert_env_cleared();
    }

    #[test]
    fn from_env_consumes_env_vars_on_success_with_zero_fds() {
        let _g = ENV_MUTEX.lock().unwrap();
        // LISTEN_FDS=0 succeeds (no fds to extract) and should still clear env.
        set_env(Some("0"), Some("nothing"), Some("12345"));

        let _ = ActivatedFds::from_env().unwrap();
        assert_env_cleared();
    }

    #[test]
    fn from_env_consumes_env_vars_on_error() {
        let _g = ENV_MUTEX.lock().unwrap();
        set_env(Some("bogus"), None, None);

        let _ = ActivatedFds::from_env().unwrap_err();
        assert_env_cleared();
    }

    #[test]
    fn second_call_returns_empty() {
        let _g = ENV_MUTEX.lock().unwrap();
        set_env(Some("0"), None, None);

        let first = ActivatedFds::from_env().unwrap();
        assert!(first.is_empty());

        // Env already cleared by first call; second call sees no env, returns empty.
        let second = ActivatedFds::from_env().unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn take_first_unknown_returns_none_when_empty() {
        let mut fds = ActivatedFds {
            fds: HashMap::new(),
        };
        assert!(fds.take_first_unknown().is_none());
        assert!(fds.take("tcp").is_none());
    }

    // Regression coverage for the exposure-guard inputs (qemu auth audit).

    #[test]
    fn unix_only_fd_provides_no_tcp_and_no_routable_inet() {
        // The Finding-1 case: a unix-only activated fd set must report NO TCP
        // source (so the guard falls back to checking listen_addr) and NO
        // routable inet listener (AF_UNIX is auth-terminated at the relay).
        let path = std::env::temp_dir().join(format!("lamco-sa-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let af = ActivatedFds {
            fds: HashMap::from([("unix".to_string(), OwnedFd::from(unix))]),
        };
        assert!(!af.provides_tcp_listener());
        assert!(!af.has_routable_inet_listener());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn routable_tcp_fd_is_flagged_routable() {
        let tcp = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let af = ActivatedFds {
            fds: HashMap::from([("tcp".to_string(), OwnedFd::from(tcp))]),
        };
        assert!(af.provides_tcp_listener());
        assert!(af.has_routable_inet_listener());
    }

    #[test]
    fn loopback_tcp_fd_is_a_tcp_source_but_not_routable() {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let af = ActivatedFds {
            fds: HashMap::from([("tcp".to_string(), OwnedFd::from(tcp))]),
        };
        assert!(af.provides_tcp_listener());
        assert!(!af.has_routable_inet_listener());
    }
}
