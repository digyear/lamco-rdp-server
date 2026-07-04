//! Sensor Registry — runtime registration and snapshot aggregation

use std::sync::Arc;

use parking_lot::RwLock;
use tracing::info;

use super::{HealthSensor, ProtocolLayer, SensorSnapshot, SignalDescriptor};

/// Registry for version-adaptive health sensors.
///
/// Sensors are registered after protocol negotiation and unregistered
/// when protocols tear down. The registry produces aggregated snapshots
/// from all registered sensors.
pub struct SensorRegistry {
    sensors: RwLock<Vec<Arc<dyn HealthSensor>>>,
}

impl SensorRegistry {
    pub fn new() -> Self {
        Self {
            sensors: RwLock::new(Vec::new()),
        }
    }

    /// Register a sensor after protocol negotiation.
    pub fn register(&self, sensor: Arc<dyn HealthSensor>) {
        let layer = sensor.layer();
        let version = sensor.version().to_string();
        let signal_count = sensor.available_signals().len();
        info!(
            "Sensor registered: {} v{} ({} signals)",
            layer, version, signal_count
        );
        self.sensors.write().push(sensor);
    }

    /// Unregister all sensors for a protocol layer.
    pub fn unregister(&self, layer: ProtocolLayer) {
        let mut sensors = self.sensors.write();
        let before = sensors.len();
        sensors.retain(|s| s.layer() != layer);
        let removed = before - sensors.len();
        if removed > 0 {
            info!("Unregistered {} sensor(s) for {}", removed, layer);
        }
    }

    /// Collect snapshots from all registered sensors.
    pub fn snapshot_all(&self) -> Vec<SensorSnapshot> {
        self.sensors.read().iter().map(|s| s.snapshot()).collect()
    }

    /// List all available signals across all registered sensors.
    pub fn available_signals(&self) -> Vec<(ProtocolLayer, SignalDescriptor)> {
        self.sensors
            .read()
            .iter()
            .flat_map(|s| {
                let layer = s.layer();
                s.available_signals()
                    .iter()
                    .map(move |sig| (layer, sig.clone()))
            })
            .collect()
    }

    /// Number of registered sensors
    pub fn sensor_count(&self) -> usize {
        self.sensors.read().len()
    }
}

impl Default for SensorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::health::sensors::{MetricType, signal};

    struct MockSensor {
        layer: ProtocolLayer,
        version: String,
        signals: Vec<SignalDescriptor>,
    }

    impl HealthSensor for MockSensor {
        fn layer(&self) -> ProtocolLayer {
            self.layer
        }
        fn version(&self) -> &str {
            &self.version
        }
        fn available_signals(&self) -> &[SignalDescriptor] {
            &self.signals
        }
        fn snapshot(&self) -> SensorSnapshot {
            SensorSnapshot {
                layer: self.layer,
                version: self.version.clone(),
                values: HashMap::new(),
                available_signals: self.signals.iter().map(|s| s.name.clone()).collect(),
            }
        }
    }

    #[test]
    fn register_and_snapshot() {
        let registry = SensorRegistry::new();
        let sensor = Arc::new(MockSensor {
            layer: ProtocolLayer::Egfx,
            version: "V10.5".into(),
            signals: vec![signal("queue_depth", MetricType::Gauge, "Queue depth")],
        });

        registry.register(sensor);
        assert_eq!(registry.sensor_count(), 1);

        let snapshots = registry.snapshot_all();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].layer, ProtocolLayer::Egfx);
    }

    #[test]
    fn unregister_by_layer() {
        let registry = SensorRegistry::new();
        registry.register(Arc::new(MockSensor {
            layer: ProtocolLayer::Egfx,
            version: "V10.5".into(),
            signals: vec![],
        }));
        registry.register(Arc::new(MockSensor {
            layer: ProtocolLayer::Encoder,
            version: "openh264".into(),
            signals: vec![],
        }));

        assert_eq!(registry.sensor_count(), 2);
        registry.unregister(ProtocolLayer::Egfx);
        assert_eq!(registry.sensor_count(), 1);

        let snapshots = registry.snapshot_all();
        assert_eq!(snapshots[0].layer, ProtocolLayer::Encoder);
    }
}
