//! Mutter Sensor — Mutter Direct API health with version tracking

use std::collections::HashMap;

use super::{HealthSensor, MetricType, ProtocolLayer, SensorSnapshot, SignalDescriptor, signal};

/// Mutter Direct API sensor tracking ScreenCast and RemoteDesktop versions.
///
/// Unlike the Portal sensor which tracks portal interface versions,
/// this sensor tracks the Mutter-specific D-Bus API versions which
/// determine feature availability (EIS input, clipboard, etc.).
pub struct MutterSensor {
    screencast_version: i32,
    remote_desktop_version: i32,
    signals: Vec<SignalDescriptor>,
    /// Whether the Mutter session is valid
    session_valid: std::sync::atomic::AtomicBool,
    /// Supported device types bitmask
    supported_device_types: u32,
}

impl MutterSensor {
    pub fn new(
        screencast_version: i32,
        remote_desktop_version: i32,
        supported_device_types: u32,
    ) -> Self {
        let signals = vec![
            signal(
                "mutter_session_valid",
                MetricType::Gauge,
                "Whether Mutter session is valid (1=yes, 0=no)",
            ),
            signal(
                "mutter_screencast_version",
                MetricType::Gauge,
                "Mutter ScreenCast API version",
            ),
            signal(
                "mutter_remote_desktop_version",
                MetricType::Gauge,
                "Mutter RemoteDesktop API version",
            ),
            signal(
                "mutter_supported_device_types",
                MetricType::Gauge,
                "Bitmask of supported input device types",
            ),
        ];

        Self {
            screencast_version,
            remote_desktop_version,
            signals,
            session_valid: std::sync::atomic::AtomicBool::new(true),
            supported_device_types,
        }
    }

    /// Mark session as invalid (call on Mutter session destruction).
    pub fn set_session_invalid(&self) {
        self.session_valid
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

impl HealthSensor for MutterSensor {
    fn layer(&self) -> ProtocolLayer {
        ProtocolLayer::Portal
    }

    fn version(&self) -> &str {
        // Use RemoteDesktop version as the primary version indicator.
        match self.remote_desktop_version {
            1 => "mutter-rd-1",
            2 => "mutter-rd-2",
            3 => "mutter-rd-3",
            _ => "mutter-rd-unknown",
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
        values.insert("mutter_session_valid".into(), if valid { 1.0 } else { 0.0 });
        values.insert(
            "mutter_screencast_version".into(),
            f64::from(self.screencast_version),
        );
        values.insert(
            "mutter_remote_desktop_version".into(),
            f64::from(self.remote_desktop_version),
        );
        values.insert(
            "mutter_supported_device_types".into(),
            f64::from(self.supported_device_types),
        );

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
    fn mutter_sensor_basic() {
        let sensor = MutterSensor::new(4, 2, 7);
        assert_eq!(sensor.available_signals().len(), 4);
        assert_eq!(sensor.version(), "mutter-rd-2");

        let snap = sensor.snapshot();
        assert_eq!(snap.values["mutter_session_valid"], 1.0);
        assert_eq!(snap.values["mutter_screencast_version"], 4.0);
        assert_eq!(snap.values["mutter_remote_desktop_version"], 2.0);
    }

    #[test]
    fn mutter_sensor_invalidation() {
        let sensor = MutterSensor::new(4, 2, 7);
        sensor.set_session_invalid();
        let snap = sensor.snapshot();
        assert_eq!(snap.values["mutter_session_valid"], 0.0);
    }
}
