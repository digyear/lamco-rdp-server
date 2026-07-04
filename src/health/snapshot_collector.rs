//! Snapshot Collector — assembles PerformanceSnapshot from live data sources
//!
//! **Execution Path:** Cross-cutting (all session strategies)
//! **Status:** Active (v1.5.0+)
//!
//! Holds Arc references to all performance data sources and produces
//! a unified `PerformanceSnapshot` on demand. Constructed in `server/mod.rs`
//! alongside the health monitor. Sources are set incrementally as the
//! session establishes (EGFX negotiation, encoder creation, etc.).

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use parking_lot::RwLock;

use super::{
    performance::{
        EgfxSnapshot, EncoderSnapshot, FpsSnapshot, LatencySnapshot, MetricsSnapshot,
        PerformanceSnapshot,
    },
    sensors::registry::SensorRegistry,
};
use crate::runtime::metrics::{self, MetricsCollector};

/// Produces point-in-time PerformanceSnapshot from live data sources.
///
/// Not all sources are available at construction time — EGFX and encoder
/// sources are set after protocol negotiation. Missing sources produce
/// default snapshot values.
pub struct SnapshotCollector {
    /// Core metrics (always present once server starts)
    metrics: Arc<MetricsCollector>,

    /// EGFX state (set after GraphicsPipeline negotiation)
    egfx_state: Arc<RwLock<EgfxSnapshot>>,

    /// Encoder stats (set when encoder is created)
    encoder_state: Arc<RwLock<Option<EncoderSnapshot>>>,

    /// FPS controller state (set during session setup)
    fps_state: Arc<RwLock<FpsSnapshot>>,

    /// Latency governor state (set during session setup)
    latency_state: Arc<RwLock<LatencySnapshot>>,

    /// Version-adaptive sensor registry (set during session setup)
    sensor_registry: Option<Arc<SensorRegistry>>,

    /// Server start time (for uptime calculation)
    start_time: Instant,
}

impl SnapshotCollector {
    pub fn new(metrics: Arc<MetricsCollector>, sensor_registry: Arc<SensorRegistry>) -> Self {
        Self {
            metrics,
            egfx_state: Arc::new(RwLock::new(EgfxSnapshot::default())),
            encoder_state: Arc::new(RwLock::new(None)),
            fps_state: Arc::new(RwLock::new(FpsSnapshot::default())),
            latency_state: Arc::new(RwLock::new(LatencySnapshot::default())),
            sensor_registry: Some(sensor_registry),
            start_time: Instant::now(),
        }
    }

    /// Produce a point-in-time snapshot from all data sources.
    pub fn snapshot(&self) -> PerformanceSnapshot {
        let metrics_snap = self.metrics.snapshot();

        PerformanceSnapshot {
            timestamp: SystemTime::now(),
            uptime: self.start_time.elapsed(),
            fps: self.fps_state.read().clone(),
            latency: self.latency_state.read().clone(),
            egfx: self.egfx_state.read().clone(),
            encoder: self.encoder_state.read().clone(),
            metrics: MetricsSnapshot {
                active_connections: metrics_snap
                    .counters
                    .get(metrics::metric_names::CONNECTIONS_ACTIVE)
                    .copied()
                    .or_else(|| {
                        metrics_snap
                            .gauges
                            .get(metrics::metric_names::CONNECTIONS_ACTIVE)
                            .map(|v| *v as u64)
                    })
                    .unwrap_or(0),
                bytes_sent: metrics_snap
                    .counters
                    .get(metrics::metric_names::BYTES_SENT)
                    .copied()
                    .unwrap_or(0),
                bytes_received: metrics_snap
                    .counters
                    .get(metrics::metric_names::BYTES_RECEIVED)
                    .copied()
                    .unwrap_or(0),
                frames_received: metrics_snap
                    .counters
                    .get(metrics::metric_names::FRAMES_RECEIVED)
                    .copied()
                    .unwrap_or(0),
                frames_dropped: metrics_snap
                    .counters
                    .get(metrics::metric_names::FRAMES_DROPPED)
                    .copied()
                    .unwrap_or(0),
            },
            sensor_snapshots: self
                .sensor_registry
                .as_ref()
                .map(|r| r.snapshot_all())
                .unwrap_or_default(),
        }
    }

    /// Get a handle for updating EGFX state from the handler callbacks
    pub fn egfx_state(&self) -> Arc<RwLock<EgfxSnapshot>> {
        Arc::clone(&self.egfx_state)
    }

    /// Get a handle for updating encoder state from the encoder backends
    pub fn encoder_state(&self) -> Arc<RwLock<Option<EncoderSnapshot>>> {
        Arc::clone(&self.encoder_state)
    }

    /// Get a handle for updating FPS controller state
    pub fn fps_state(&self) -> Arc<RwLock<FpsSnapshot>> {
        Arc::clone(&self.fps_state)
    }

    /// Get a handle for updating latency governor state
    pub fn latency_state(&self) -> Arc<RwLock<LatencySnapshot>> {
        Arc::clone(&self.latency_state)
    }

    /// Elapsed time since the collector was created
    pub fn uptime(&self) -> Duration {
        self.start_time.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_returns_defaults_when_no_sources_set() {
        let metrics = Arc::new(MetricsCollector::new());
        let collector = SnapshotCollector::new(metrics, Arc::new(SensorRegistry::new()));

        let snap = collector.snapshot();
        assert!(snap.uptime.as_millis() < 100);
        assert_eq!(snap.fps.current_fps, 30);
        assert!(!snap.egfx.channel_ready);
        assert!(snap.encoder.is_none());
    }

    #[test]
    fn snapshot_reflects_updated_egfx_state() {
        let metrics = Arc::new(MetricsCollector::new());
        let collector = SnapshotCollector::new(metrics, Arc::new(SensorRegistry::new()));

        // Simulate EGFX negotiation
        {
            let egfx_handle = collector.egfx_state();
            let mut egfx = egfx_handle.write();
            egfx.channel_ready = true;
            egfx.avc420_enabled = true;
            egfx.negotiated_version = Some("V10.5".into());
            egfx.queue_depth = 2;
        }

        let snap = collector.snapshot();
        assert!(snap.egfx.channel_ready);
        assert!(snap.egfx.avc420_enabled);
        assert_eq!(snap.egfx.negotiated_version.as_deref(), Some("V10.5"));
        assert_eq!(snap.egfx.queue_depth, 2);
    }

    #[test]
    fn snapshot_reflects_encoder_stats() {
        let metrics = Arc::new(MetricsCollector::new());
        let collector = SnapshotCollector::new(metrics, Arc::new(SensorRegistry::new()));

        {
            let enc_handle = collector.encoder_state();
            let mut enc = enc_handle.write();
            *enc = Some(EncoderSnapshot {
                backend: "vaapi".into(),
                fps: 29.7,
                bitrate_kbps: 4500,
                ..EncoderSnapshot::default()
            });
        }

        let snap = collector.snapshot();
        let enc = snap.encoder.unwrap();
        assert_eq!(enc.backend, "vaapi");
        assert!((enc.fps - 29.7).abs() < 0.1);
    }

    #[test]
    fn snapshot_reflects_metrics_counters() {
        let metrics = Arc::new(MetricsCollector::new());
        metrics.increment_counter(metrics::metric_names::FRAMES_RECEIVED, 100);
        metrics.increment_counter(metrics::metric_names::FRAMES_DROPPED, 3);
        metrics.increment_counter(metrics::metric_names::BYTES_SENT, 1_000_000);

        let collector = SnapshotCollector::new(metrics, Arc::new(SensorRegistry::new()));
        let snap = collector.snapshot();

        assert_eq!(snap.metrics.frames_received, 100);
        assert_eq!(snap.metrics.frames_dropped, 3);
        assert_eq!(snap.metrics.bytes_sent, 1_000_000);
    }
}
