//! Encoder Sensor — wraps HardwareEncoderStats for the sensor framework

use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;

use super::{HealthSensor, MetricType, ProtocolLayer, SensorSnapshot, SignalDescriptor, signal};
use crate::health::performance::EncoderSnapshot;

/// Encoder sensor exposing fps, bitrate, encode time, and skip rate.
pub struct EncoderSensor {
    backend: String,
    signals: Vec<SignalDescriptor>,
    state: Arc<RwLock<Option<EncoderSnapshot>>>,
}

impl EncoderSensor {
    pub fn new(backend: &str, state: Arc<RwLock<Option<EncoderSnapshot>>>) -> Self {
        let signals = vec![
            signal("encoder_fps", MetricType::Gauge, "Current encoder FPS"),
            signal(
                "encoder_bitrate_kbps",
                MetricType::Gauge,
                "Current encoder bitrate (kbps)",
            ),
            signal(
                "encoder_encode_time_ms",
                MetricType::Gauge,
                "Average encode time per frame (ms)",
            ),
            signal(
                "encoder_frames_encoded",
                MetricType::Counter,
                "Total frames encoded",
            ),
            signal(
                "encoder_frames_skipped",
                MetricType::Counter,
                "Frames skipped by rate control",
            ),
        ];

        Self {
            backend: backend.into(),
            signals,
            state,
        }
    }
}

impl HealthSensor for EncoderSensor {
    fn layer(&self) -> ProtocolLayer {
        ProtocolLayer::Encoder
    }

    fn version(&self) -> &str {
        &self.backend
    }

    fn available_signals(&self) -> &[SignalDescriptor] {
        &self.signals
    }

    fn snapshot(&self) -> SensorSnapshot {
        let state = self.state.read();
        let mut values = HashMap::new();

        if let Some(ref enc) = *state {
            values.insert("encoder_fps".into(), f64::from(enc.fps));
            values.insert("encoder_bitrate_kbps".into(), f64::from(enc.bitrate_kbps));
            values.insert(
                "encoder_encode_time_ms".into(),
                f64::from(enc.avg_encode_time_ms),
            );
            values.insert("encoder_frames_encoded".into(), enc.frames_encoded as f64);
            values.insert("encoder_frames_skipped".into(), enc.frames_skipped as f64);
        }

        SensorSnapshot {
            layer: ProtocolLayer::Encoder,
            version: self.backend.clone(),
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
    fn snapshot_with_no_encoder() {
        let state = Arc::new(RwLock::new(None));
        let sensor = EncoderSensor::new("openh264", state);
        let snap = sensor.snapshot();
        assert!(snap.values.is_empty());
        assert_eq!(snap.available_signals.len(), 5);
    }

    #[test]
    fn snapshot_with_active_encoder() {
        let state = Arc::new(RwLock::new(Some(EncoderSnapshot {
            backend: "vaapi".into(),
            fps: 29.5,
            bitrate_kbps: 4800,
            avg_encode_time_ms: 2.3,
            frames_encoded: 500,
            frames_skipped: 10,
            ..EncoderSnapshot::default()
        })));
        let sensor = EncoderSensor::new("vaapi", state);
        let snap = sensor.snapshot();

        assert!((snap.values["encoder_fps"] - 29.5).abs() < 0.1);
        assert_eq!(snap.values["encoder_bitrate_kbps"], 4800.0);
        assert_eq!(snap.values["encoder_frames_encoded"], 500.0);
    }
}
