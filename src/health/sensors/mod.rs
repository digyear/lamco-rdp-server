//! Version-Adaptive Health Sensors
//!
//! **Execution Path:** Cross-cutting (all session strategies)
//! **Status:** Active (v1.5.0+)
//!
//! Each protocol layer gets a sensor that registers only the signals
//! its negotiated version supports. Sensors are added/removed at
//! runtime as protocols negotiate and tear down.

pub mod egfx;
pub mod encoder;
pub mod mutter;
pub mod pipewire;
pub mod portal;
pub mod registry;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Protocol layer identity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtocolLayer {
    Egfx,
    Encoder,
    PipeWire,
    Portal,
}

impl std::fmt::Display for ProtocolLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Egfx => write!(f, "EGFX"),
            Self::Encoder => write!(f, "Encoder"),
            Self::PipeWire => write!(f, "PipeWire"),
            Self::Portal => write!(f, "Portal"),
        }
    }
}

/// Type of metric a signal produces
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricType {
    /// Monotonically increasing counter
    Counter,
    /// Instantaneous value
    Gauge,
    /// Distribution of values
    Histogram,
}

/// Description of a single signal a sensor can produce
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalDescriptor {
    /// Metric name (will be prefixed with lamco_rdp_)
    pub name: String,
    /// Type of metric
    pub metric_type: MetricType,
    /// Human-readable description
    pub description: String,
    /// Minimum protocol version required for this signal
    pub min_version: Option<String>,
}

/// Point-in-time snapshot from a single sensor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorSnapshot {
    /// Which protocol layer this sensor monitors
    pub layer: ProtocolLayer,
    /// Negotiated protocol version
    pub version: String,
    /// Current signal values (name → value)
    pub values: HashMap<String, f64>,
    /// Which signals are available at this version
    pub available_signals: Vec<String>,
}

/// Trait for version-adaptive health sensors.
///
/// Each sensor knows its protocol layer and which signals are
/// available at the negotiated version. The registry calls
/// `snapshot()` to collect current values.
pub trait HealthSensor: Send + Sync {
    /// Which protocol layer this sensor monitors
    fn layer(&self) -> ProtocolLayer;

    /// Negotiated protocol version string
    fn version(&self) -> &str;

    /// Signals available at this version
    fn available_signals(&self) -> &[SignalDescriptor];

    /// Produce a point-in-time snapshot of current signal values
    fn snapshot(&self) -> SensorSnapshot;
}

/// Helper to build a SignalDescriptor
pub fn signal(name: &str, metric_type: MetricType, description: &str) -> SignalDescriptor {
    SignalDescriptor {
        name: name.into(),
        metric_type,
        description: description.into(),
        min_version: None,
    }
}

/// Helper to build a version-gated SignalDescriptor
pub fn versioned_signal(
    name: &str,
    metric_type: MetricType,
    description: &str,
    min_version: &str,
) -> SignalDescriptor {
    SignalDescriptor {
        name: name.into(),
        metric_type,
        description: description.into(),
        min_version: Some(min_version.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_descriptor_serializes() {
        let sig = signal("test_metric", MetricType::Gauge, "A test metric");
        let json = serde_json::to_string(&sig).unwrap();
        assert!(json.contains("test_metric"));
        assert!(json.contains("Gauge"));
    }
}
