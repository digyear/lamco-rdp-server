//! Portal Sensor — XDG Portal session health with version-gated features

use std::collections::HashMap;

use super::{
    HealthSensor, MetricType, ProtocolLayer, SensorSnapshot, SignalDescriptor, signal,
    versioned_signal,
};

/// Portal sensor tracking session validity and version-gated features.
pub struct PortalSensor {
    portal_version: u32,
    signals: Vec<SignalDescriptor>,
    /// Whether the portal session is valid (1=yes, 0=no)
    session_valid: std::sync::atomic::AtomicBool,
    /// Whether clipboard is supported (Portal v2+)
    clipboard_supported: bool,
    /// Whether session restore is supported (Portal v4+)
    restore_supported: bool,
}

impl PortalSensor {
    pub fn new(portal_version: u32) -> Self {
        let mut signals = vec![
            signal(
                "portal_session_valid",
                MetricType::Gauge,
                "Whether portal session is valid (1=yes, 0=no)",
            ),
            signal(
                "portal_version",
                MetricType::Gauge,
                "Portal ScreenCast/RemoteDesktop API version",
            ),
        ];

        let clipboard_supported = portal_version >= 2;
        if clipboard_supported {
            signals.push(versioned_signal(
                "portal_clipboard_available",
                MetricType::Gauge,
                "Whether portal clipboard is available",
                "2",
            ));
        }

        let restore_supported = portal_version >= 4;
        if restore_supported {
            signals.push(versioned_signal(
                "portal_restore_available",
                MetricType::Gauge,
                "Whether session restore tokens are supported",
                "4",
            ));
        }

        Self {
            portal_version,
            signals,
            session_valid: std::sync::atomic::AtomicBool::new(true),
            clipboard_supported,
            restore_supported,
        }
    }

    /// Mark session as invalid (call on session destruction)
    pub fn set_session_invalid(&self) {
        self.session_valid
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

impl HealthSensor for PortalSensor {
    fn layer(&self) -> ProtocolLayer {
        ProtocolLayer::Portal
    }

    fn version(&self) -> &str {
        // Return a static str for common versions
        match self.portal_version {
            1 => "1",
            2 => "2",
            3 => "3",
            4 => "4",
            5 => "5",
            _ => "unknown",
        }
    }

    fn available_signals(&self) -> &[SignalDescriptor] {
        &self.signals
    }

    fn snapshot(&self) -> SensorSnapshot {
        let mut values = HashMap::new();

        let valid = self
            .session_valid
            .load(std::sync::atomic::Ordering::Relaxed);
        values.insert("portal_session_valid".into(), if valid { 1.0 } else { 0.0 });
        values.insert("portal_version".into(), f64::from(self.portal_version));

        if self.clipboard_supported {
            values.insert("portal_clipboard_available".into(), 1.0);
        }
        if self.restore_supported {
            values.insert("portal_restore_available".into(), 1.0);
        }

        SensorSnapshot {
            layer: ProtocolLayer::Portal,
            version: self.version().into(),
            values,
            available_signals: self.signals.iter().map(|s| s.name.clone()).collect(),
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "tests assert exact float values converted from integer sensor counters"
)]
mod tests {
    use super::*;

    #[test]
    fn portal_v1_minimal_signals() {
        let sensor = PortalSensor::new(1);
        assert_eq!(sensor.available_signals().len(), 2);
    }

    #[test]
    fn portal_v4_full_signals() {
        let sensor = PortalSensor::new(4);
        assert_eq!(sensor.available_signals().len(), 4);
        assert!(
            sensor
                .available_signals()
                .iter()
                .any(|s| s.name == "portal_clipboard_available")
        );
        assert!(
            sensor
                .available_signals()
                .iter()
                .any(|s| s.name == "portal_restore_available")
        );
    }

    #[test]
    fn session_invalidation_reflected() {
        let sensor = PortalSensor::new(4);
        let snap1 = sensor.snapshot();
        assert_eq!(snap1.values["portal_session_valid"], 1.0);

        sensor.set_session_invalid();
        let snap2 = sensor.snapshot();
        assert_eq!(snap2.values["portal_session_valid"], 0.0);
    }
}
