//! Authentication of the RDCleanPath `proxy_auth` token on the direct
//! WebSocket entry path.
//!
//! lamco-qemu-rdp follows the two-layer hypervisor-console auth model (see
//! `docs/design/security/qemu-rdp-auth-recalibration-2026-05-30/T2-comparable-console-auth.md`):
//! in the pveproxy-fronted deployment the PVE login ticket and the `VM.Console`
//! ACL are enforced at the proxy, and the RDP backend is a node-local unix
//! socket that is not network-reachable. This module secures the *other* path:
//! a client that speaks RDCleanPath directly to the in-process WebSocket
//! listener, where the RDCleanPath `proxy_auth` field carries a deployment
//! shared secret. It deliberately does NOT validate PVE tickets — that stays at
//! pveproxy (we do not reimplement the PVE RSA ticket verification).

use subtle::ConstantTimeEq as _;

/// Reason a `proxy_auth` token was rejected. Carries no secret material, so it
/// is safe to log.
#[derive(Debug, Clone, Copy)]
pub struct ProxyAuthRejected {
    reason: &'static str,
}

impl ProxyAuthRejected {
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl core::fmt::Display for ProxyAuthRejected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "proxy_auth rejected: {}", self.reason)
    }
}

/// Validates the RDCleanPath `proxy_auth` token before a connection is handed
/// to the RDP acceptor on the direct WebSocket path.
///
/// Implementations MUST fail closed: any uncertainty rejects the connection.
pub trait ProxyAuthValidator: Send + Sync {
    fn validate(&self, proxy_auth: &[u8], destination: &str) -> Result<(), ProxyAuthRejected>;
}

/// Rejects every connection. This is the default when no validator is
/// configured, so an unconfigured listener is closed rather than open.
pub struct DenyAll;

impl ProxyAuthValidator for DenyAll {
    fn validate(&self, _proxy_auth: &[u8], _destination: &str) -> Result<(), ProxyAuthRejected> {
        Err(ProxyAuthRejected {
            reason: "no proxy_auth validator configured",
        })
    }
}

/// Constant-time shared-secret bearer token.
///
/// SECURITY: the secret authenticates *possession of the token*, not a per-user
/// or per-VM identity, so it is strictly weaker than the PVE ticket enforced at
/// pveproxy. It MUST therefore only be used on a TLS-terminated listener (the
/// WSS layer carries the token) and be provisioned out of band from a
/// permissions-restricted source. The strong per-user, per-VM, short-lived path
/// remains pveproxy + the PVE `VM.Console` ticket.
pub struct SharedSecretValidator {
    secret: Vec<u8>,
}

impl SharedSecretValidator {
    /// Build a validator from a secret. Returns `None` for an empty secret,
    /// which would otherwise authorize every caller.
    pub fn new(secret: Vec<u8>) -> Option<Self> {
        if secret.is_empty() {
            return None;
        }
        Some(Self { secret })
    }
}

impl ProxyAuthValidator for SharedSecretValidator {
    fn validate(&self, proxy_auth: &[u8], _destination: &str) -> Result<(), ProxyAuthRejected> {
        // Constant-time over the content; `subtle` short-circuits only on the
        // length, which is not sensitive for a fixed-format token.
        if proxy_auth.ct_eq(&self.secret).into() {
            Ok(())
        } else {
            Err(ProxyAuthRejected {
                reason: "shared secret mismatch",
            })
        }
    }
}

/// Accepts every connection. An explicit opt-in for local development only —
/// never for a network-reachable deployment. Callers configuring this should
/// log a warning at startup.
pub struct AllowAllInsecure;

impl ProxyAuthValidator for AllowAllInsecure {
    fn validate(&self, _proxy_auth: &[u8], _destination: &str) -> Result<(), ProxyAuthRejected> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"correct-horse-battery-staple";

    #[test]
    fn deny_all_rejects_everything() {
        let v = DenyAll;
        assert!(v.validate(b"", "vm-128").is_err());
        assert!(v.validate(SECRET, "vm-128").is_err());
    }

    #[test]
    fn shared_secret_accepts_exact_match() {
        let v = SharedSecretValidator::new(SECRET.to_vec()).unwrap();
        assert!(v.validate(SECRET, "vm-128").is_ok());
    }

    #[test]
    fn shared_secret_rejects_mismatch_and_empty() {
        let v = SharedSecretValidator::new(SECRET.to_vec()).unwrap();
        assert!(v.validate(b"wrong", "vm-128").is_err());
        assert!(v.validate(b"", "vm-128").is_err());
        // A prefix of the secret must not be accepted.
        assert!(v.validate(b"correct-horse", "vm-128").is_err());
    }

    #[test]
    fn empty_secret_is_rejected_at_construction() {
        assert!(SharedSecretValidator::new(Vec::new()).is_none());
    }

    #[test]
    fn allow_all_insecure_accepts_everything() {
        let v = AllowAllInsecure;
        assert!(v.validate(b"", "vm-128").is_ok());
        assert!(v.validate(b"anything", "vm-128").is_ok());
    }
}
