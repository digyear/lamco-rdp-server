//! EGFX Sensor — EGFX pipeline health with version-gated QoE signals

use std::{collections::HashMap, sync::Arc};

use parking_lot::RwLock;

use super::{
    HealthSensor, MetricType, ProtocolLayer, SensorSnapshot, SignalDescriptor, signal,
    versioned_signal,
};
use crate::health::performance::EgfxSnapshot;

/// EGFX pipeline sensor with version-gated signals.
///
/// Frame ack and queue depth are available on all EGFX versions.
/// QoE (client decode+render time) requires V10.4+.
pub struct EgfxSensor {
    version: String,
    signals: Vec<SignalDescriptor>,
    state: Arc<RwLock<EgfxSnapshot>>,
}

impl EgfxSensor {
    pub fn new(version: &str, state: Arc<RwLock<EgfxSnapshot>>) -> Self {
        let mut signals = vec![
            signal(
                "egfx_queue_depth",
                MetricType::Gauge,
                "Frame ack queue depth",
            ),
            signal(
                "egfx_frame_acks_total",
                MetricType::Counter,
                "Total frame acknowledgements",
            ),
            signal(
                "egfx_channel_ready",
                MetricType::Gauge,
                "Whether EGFX channel is ready (1=yes, 0=no)",
            ),
        ];

        // QoE metrics require V10.4+ (MS-RDPEGFX Section 2.2.3.4)
        let version_supports_qoe = matches!(version, "V10.4" | "V10.5" | "V10.6" | "V10.7");
        if version_supports_qoe {
            signals.push(versioned_signal(
                "egfx_client_decode_render_us",
                MetricType::Histogram,
                "Client decode+render time (microseconds)",
                "V10.4",
            ));
        }

        Self {
            version: version.into(),
            signals,
            state,
        }
    }
}

impl HealthSensor for EgfxSensor {
    fn layer(&self) -> ProtocolLayer {
        ProtocolLayer::Egfx
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn available_signals(&self) -> &[SignalDescriptor] {
        &self.signals
    }

    fn snapshot(&self) -> SensorSnapshot {
        let state = self.state.read();
        let mut values = HashMap::new();

        values.insert("egfx_queue_depth".into(), f64::from(state.queue_depth));
        values.insert("egfx_frame_acks_total".into(), state.frame_acks as f64);
        values.insert(
            "egfx_channel_ready".into(),
            if state.channel_ready { 1.0 } else { 0.0 },
        );

        if let Some(decode_us) = state.client_decode_render_us {
            values.insert("egfx_client_decode_render_us".into(), decode_us as f64);
        }

        SensorSnapshot {
            layer: ProtocolLayer::Egfx,
            version: self.version.clone(),
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
    fn v81_has_no_qoe_signal() {
        let state = Arc::new(RwLock::new(EgfxSnapshot::default()));
        let sensor = EgfxSensor::new("V8.1", state);
        assert_eq!(sensor.available_signals().len(), 3);
        assert!(
            !sensor
                .available_signals()
                .iter()
                .any(|s| s.name.contains("decode_render"))
        );
    }

    #[test]
    fn v105_has_qoe_signal() {
        let state = Arc::new(RwLock::new(EgfxSnapshot::default()));
        let sensor = EgfxSensor::new("V10.5", state);
        assert_eq!(sensor.available_signals().len(), 4);
        assert!(
            sensor
                .available_signals()
                .iter()
                .any(|s| s.name.contains("decode_render"))
        );
    }

    #[test]
    fn snapshot_reflects_state() {
        let state = Arc::new(RwLock::new(EgfxSnapshot {
            channel_ready: true,
            queue_depth: 3,
            frame_acks: 100,
            client_decode_render_us: Some(4500),
            ..EgfxSnapshot::default()
        }));
        let sensor = EgfxSensor::new("V10.5", state);
        let snap = sensor.snapshot();

        assert_eq!(snap.values["egfx_channel_ready"], 1.0);
        assert_eq!(snap.values["egfx_queue_depth"], 3.0);
        assert_eq!(snap.values["egfx_frame_acks_total"], 100.0);
        assert_eq!(snap.values["egfx_client_decode_render_us"], 4500.0);
    }
}
