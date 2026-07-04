//! PipeWire Sensor — stream state with version-gated buffer metrics

use std::collections::HashMap;

use super::{
    HealthSensor, MetricType, ProtocolLayer, SensorSnapshot, SignalDescriptor, signal,
    versioned_signal,
};

/// PipeWire sensor with version-gated signals.
///
/// Basic stream state is always available. Buffer pressure and quantum
/// size require newer PipeWire versions.
pub struct PipeWireSensor {
    #[expect(dead_code, reason = "Stored for future version-gated runtime checks")]
    version: (u32, u32, u32),
    version_str: String,
    signals: Vec<SignalDescriptor>,
    /// Current stream state (0=error, 1=paused, 2=streaming)
    stream_state: std::sync::atomic::AtomicU32,
    /// Total frames processed from PipeWire
    frames_processed: std::sync::atomic::AtomicU64,
}

impl PipeWireSensor {
    pub fn new(pw_version: (u32, u32, u32)) -> Self {
        let version_str = format!("{}.{}.{}", pw_version.0, pw_version.1, pw_version.2);

        let mut signals = vec![
            signal(
                "pipewire_stream_state",
                MetricType::Gauge,
                "PipeWire stream state (0=error, 1=paused, 2=streaming)",
            ),
            signal(
                "pipewire_frames_processed",
                MetricType::Counter,
                "Total frames from PipeWire",
            ),
        ];

        // Buffer pressure available in PW 0.3.50+
        if pw_version >= (0, 3, 50) {
            signals.push(versioned_signal(
                "pipewire_buffer_pressure",
                MetricType::Gauge,
                "Buffer pressure (queued / (queued + available))",
                "0.3.50",
            ));
        }

        // Quantum size available in PW 1.1.0+
        if pw_version >= (1, 1, 0) {
            signals.push(versioned_signal(
                "pipewire_quantum_size",
                MetricType::Gauge,
                "Frames per cycle",
                "1.1.0",
            ));
        }

        Self {
            version: pw_version,
            version_str,
            signals,
            stream_state: std::sync::atomic::AtomicU32::new(0),
            frames_processed: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Update stream state (call from PipeWire callbacks)
    pub fn set_stream_state(&self, state: u32) {
        self.stream_state
            .store(state, std::sync::atomic::Ordering::Relaxed);
    }

    /// Increment frames processed
    pub fn increment_frames(&self) {
        self.frames_processed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl HealthSensor for PipeWireSensor {
    fn layer(&self) -> ProtocolLayer {
        ProtocolLayer::PipeWire
    }

    fn version(&self) -> &str {
        &self.version_str
    }

    fn available_signals(&self) -> &[SignalDescriptor] {
        &self.signals
    }

    fn snapshot(&self) -> SensorSnapshot {
        let mut values = HashMap::new();

        values.insert(
            "pipewire_stream_state".into(),
            f64::from(self.stream_state.load(std::sync::atomic::Ordering::Relaxed)),
        );
        values.insert(
            "pipewire_frames_processed".into(),
            self.frames_processed
                .load(std::sync::atomic::Ordering::Relaxed) as f64,
        );

        SensorSnapshot {
            layer: ProtocolLayer::PipeWire,
            version: self.version_str.clone(),
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
    fn old_pipewire_fewer_signals() {
        let sensor = PipeWireSensor::new((0, 3, 49));
        assert_eq!(sensor.available_signals().len(), 2);
    }

    #[test]
    fn pw_0350_adds_buffer_pressure() {
        let sensor = PipeWireSensor::new((0, 3, 50));
        assert_eq!(sensor.available_signals().len(), 3);
        assert!(
            sensor
                .available_signals()
                .iter()
                .any(|s| s.name == "pipewire_buffer_pressure")
        );
    }

    #[test]
    fn pw_110_adds_quantum() {
        let sensor = PipeWireSensor::new((1, 1, 0));
        assert_eq!(sensor.available_signals().len(), 4);
        assert!(
            sensor
                .available_signals()
                .iter()
                .any(|s| s.name == "pipewire_quantum_size")
        );
    }

    #[test]
    fn snapshot_reflects_updates() {
        let sensor = PipeWireSensor::new((0, 3, 77));
        sensor.set_stream_state(2);
        sensor.increment_frames();
        sensor.increment_frames();

        let snap = sensor.snapshot();
        assert_eq!(snap.values["pipewire_stream_state"], 2.0);
        assert_eq!(snap.values["pipewire_frames_processed"], 2.0);
    }
}
