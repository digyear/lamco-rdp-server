//! Performance Metrics and Monitoring
//!
//! Provides comprehensive metrics collection and reporting for all system components:
//! - Frame processing metrics
//! - Network throughput metrics
//! - Resource utilization metrics
//! - Latency tracking
//! - Error rate monitoring
//!
//! Metrics can be exported in various formats (JSON, Prometheus-compatible)
//! for integration with monitoring systems.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Metrics collector for the entire system
pub struct MetricsCollector {
    counters: Arc<RwLock<HashMap<String, u64>>>,
    gauges: Arc<RwLock<HashMap<String, f64>>>,
    histograms: Arc<RwLock<HashMap<String, Histogram>>>,
    start_time: Instant,
}

impl MetricsCollector {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            counters: Arc::new(RwLock::new(HashMap::new())),
            gauges: Arc::new(RwLock::new(HashMap::new())),
            histograms: Arc::new(RwLock::new(HashMap::new())),
            start_time: Instant::now(),
        }
    }

    pub fn increment_counter(&self, name: &str, value: u64) {
        let mut counters = self.counters.write();
        *counters.entry(name.to_string()).or_insert(0) += value;
    }

    pub fn set_gauge(&self, name: &str, value: f64) {
        let mut gauges = self.gauges.write();
        gauges.insert(name.to_string(), value);
    }

    pub fn record_histogram(&self, name: &str, value: f64) {
        let mut histograms = self.histograms.write();
        histograms
            .entry(name.to_string())
            .or_insert_with(Histogram::new)
            .record(value);
    }

    /// Get a counter value
    pub fn get_counter(&self, name: &str) -> Option<u64> {
        self.counters.read().get(name).copied()
    }

    /// Get a gauge value
    pub fn get_gauge(&self, name: &str) -> Option<f64> {
        self.gauges.read().get(name).copied()
    }

    /// Get histogram statistics
    pub fn get_histogram(&self, name: &str) -> Option<HistogramStats> {
        self.histograms.read().get(name).map(Histogram::stats)
    }

    /// Get all metrics as a snapshot
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            timestamp: SystemTime::now(),
            uptime: self.start_time.elapsed(),
            counters: self.counters.read().clone(),
            gauges: self.gauges.read().clone(),
            histograms: self
                .histograms
                .read()
                .iter()
                .map(|(k, v)| (k.clone(), v.stats()))
                .collect(),
        }
    }

    /// Reset all metrics
    pub fn reset(&self) {
        self.counters.write().clear();
        self.gauges.write().clear();
        self.histograms.write().clear();
    }

    /// Export metrics in OpenMetrics/Prometheus text format.
    ///
    /// All metric names are prefixed with `lamco_rdp_` namespace.
    /// Each metric includes HELP and TYPE metadata lines.
    pub fn export_prometheus(&self) -> String {
        let mut output = String::new();

        // Uptime gauge (always present)
        let uptime_secs = self.start_time.elapsed().as_secs();
        output.push_str("# HELP lamco_rdp_uptime_seconds Time since server started.\n");
        output.push_str("# TYPE lamco_rdp_uptime_seconds gauge\n");
        output.push_str(&format!("lamco_rdp_uptime_seconds {uptime_secs}\n\n"));

        for (name, value) in self.counters.read().iter() {
            let prefixed = namespace_metric(name);
            let help = metric_help(name);
            output.push_str(&format!("# HELP {prefixed} {help}\n"));
            output.push_str(&format!("# TYPE {prefixed} counter\n"));
            output.push_str(&format!("{prefixed} {value}\n\n"));
        }

        for (name, value) in self.gauges.read().iter() {
            let prefixed = namespace_metric(name);
            let help = metric_help(name);
            output.push_str(&format!("# HELP {prefixed} {help}\n"));
            output.push_str(&format!("# TYPE {prefixed} gauge\n"));
            output.push_str(&format!("{prefixed} {value}\n\n"));
        }

        for (name, histogram) in self.histograms.read().iter() {
            let prefixed = namespace_metric(name);
            let help = metric_help(name);
            let stats = histogram.stats();
            output.push_str(&format!("# HELP {prefixed} {help}\n"));
            output.push_str(&format!("# TYPE {prefixed} summary\n"));
            output.push_str(&format!("{prefixed}_count {}\n", stats.count));
            output.push_str(&format!("{prefixed}_sum {}\n", stats.sum));
            output.push_str(&format!("{prefixed}{{quantile=\"0.5\"}} {}\n", stats.p50));
            output.push_str(&format!("{prefixed}{{quantile=\"0.95\"}} {}\n", stats.p95));
            output.push_str(&format!(
                "{prefixed}{{quantile=\"0.99\"}} {}\n\n",
                stats.p99
            ));
        }

        output
    }

    /// Export metrics as JSON
    pub fn export_json(&self) -> serde_json::Result<String> {
        let snapshot = self.snapshot();
        serde_json::to_string_pretty(&snapshot)
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Rolling-window histogram for tracking value distributions.
///
/// Capped at `MAX_SAMPLES` to prevent unbounded memory growth during
/// long-running sessions. When full, oldest samples are evicted. The
/// `total_count` and `all_time_sum` fields track lifetime aggregates
/// independent of the window.
pub struct Histogram {
    values: VecDeque<f64>,
    min: f64,
    max: f64,
    /// Sum of values currently in the window (for accurate window stats)
    window_sum: f64,
    /// Lifetime count of all recorded values (monotonic)
    total_count: u64,
    /// Lifetime sum of all recorded values (monotonic)
    all_time_sum: f64,
}

/// Rolling window cap — 10k samples at 30 FPS is ~5.5 minutes of history,
/// enough for percentile accuracy without unbounded growth.
const MAX_HISTOGRAM_SAMPLES: usize = 10_000;

impl Histogram {
    fn new() -> Self {
        Self {
            values: VecDeque::new(),
            min: f64::MAX,
            max: f64::MIN,
            window_sum: 0.0,
            total_count: 0,
            all_time_sum: 0.0,
        }
    }

    fn record(&mut self, value: f64) {
        // Evict oldest sample when window is full
        if self.values.len() >= MAX_HISTOGRAM_SAMPLES
            && let Some(evicted) = self.values.pop_front()
        {
            self.window_sum -= evicted;
        }

        self.values.push_back(value);
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.window_sum += value;
        self.total_count += 1;
        self.all_time_sum += value;
    }

    fn stats(&self) -> HistogramStats {
        if self.values.is_empty() {
            return HistogramStats::default();
        }

        let count = self.values.len();
        let mean = self.window_sum / count as f64;

        let variance = self
            .values
            .iter()
            .map(|v| {
                let diff = v - mean;
                diff * diff
            })
            .sum::<f64>()
            / count as f64;
        let stddev = variance.sqrt();

        let mut sorted: Vec<f64> = self.values.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let p50 = percentile(&sorted, 0.50);
        let p95 = percentile(&sorted, 0.95);
        let p99 = percentile(&sorted, 0.99);

        HistogramStats {
            count: self.total_count,
            sum: self.all_time_sum,
            min: self.min,
            max: self.max,
            mean,
            stddev,
            p50,
            p95,
            p99,
        }
    }
}

/// Calculate percentile from sorted values using the "inclusive" method
/// (equivalent to Excel's PERCENTILE.INC or NumPy's percentile with 'lower' interpolation)
fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }

    // Use (n-1) * p formula for inclusive percentile calculation
    // This maps p=0 to first element and p=1 to last element
    let index = ((sorted_values.len() - 1) as f64 * p) as usize;
    let index = index.min(sorted_values.len() - 1);
    sorted_values[index]
}

/// Histogram statistics computed from recorded observations
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HistogramStats {
    /// Total number of observations
    pub count: u64,
    /// Sum of all observations
    pub sum: f64,
    /// Minimum observed value
    pub min: f64,
    /// Maximum observed value
    pub max: f64,
    /// Arithmetic mean of observations
    pub mean: f64,
    /// Standard deviation of observations
    pub stddev: f64,
    /// 50th percentile (median)
    pub p50: f64,
    /// 95th percentile
    pub p95: f64,
    /// 99th percentile
    pub p99: f64,
}

impl Default for HistogramStats {
    fn default() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            min: 0.0,
            max: 0.0,
            mean: 0.0,
            stddev: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
        }
    }
}

/// Point-in-time snapshot of all collected metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// When this snapshot was taken
    pub timestamp: SystemTime,
    /// Server uptime at snapshot time
    pub uptime: Duration,
    /// Counter values (monotonically increasing)
    pub counters: HashMap<String, u64>,
    /// Gauge values (current state)
    pub gauges: HashMap<String, f64>,
    /// Histogram statistics
    pub histograms: HashMap<String, HistogramStats>,
}

pub mod metric_names {
    //! Pre-defined metric names for consistency across the codebase.
    //!
    //! Use these constants instead of string literals to ensure
    //! consistent metric naming across all components.

    /// Total video frames received from PipeWire
    pub const FRAMES_RECEIVED: &str = "frames_received_total";
    /// Total video frames successfully processed
    pub const FRAMES_PROCESSED: &str = "frames_processed_total";
    /// Total video frames dropped (queue full, timeout, etc.)
    pub const FRAMES_DROPPED: &str = "frames_dropped_total";
    /// Frame processing time histogram (milliseconds)
    pub const FRAME_PROCESSING_TIME_MS: &str = "frame_processing_time_ms";

    /// Total format conversions performed
    pub const CONVERSIONS_TOTAL: &str = "conversions_total";
    /// Conversion time histogram (milliseconds)
    pub const CONVERSION_TIME_MS: &str = "conversion_time_ms";
    /// Total bytes converted
    pub const CONVERSION_BYTES: &str = "conversion_bytes_total";

    /// Total frames dispatched to RDP clients
    pub const FRAMES_DISPATCHED: &str = "frames_dispatched_total";
    /// Current frames waiting in dispatch queue
    pub const FRAMES_QUEUED: &str = "frames_queued";
    /// Dispatch time histogram (microseconds)
    pub const DISPATCH_TIME_US: &str = "dispatch_time_us";

    /// Total bytes sent to RDP clients
    pub const BYTES_SENT: &str = "bytes_sent_total";
    /// Total bytes received from RDP clients
    pub const BYTES_RECEIVED: &str = "bytes_received_total";
    /// Total RDP packets sent
    pub const PACKETS_SENT: &str = "packets_sent_total";
    /// Total RDP packets received
    pub const PACKETS_RECEIVED: &str = "packets_received_total";
    /// Total network errors encountered
    pub const NETWORK_ERRORS: &str = "network_errors_total";

    /// Currently active RDP connections
    pub const CONNECTIONS_ACTIVE: &str = "connections_active";
    /// Total RDP connections since server start
    pub const CONNECTIONS_TOTAL: &str = "connections_total";
    /// Total connection errors (auth failures, protocol errors, etc.)
    pub const CONNECTION_ERRORS: &str = "connection_errors_total";

    /// Current CPU usage percentage
    pub const CPU_USAGE: &str = "cpu_usage_percent";
    /// Current memory usage in bytes
    pub const MEMORY_USAGE: &str = "memory_usage_bytes";
    /// Current memory usage as percentage of system total
    pub const MEMORY_USAGE_PERCENT: &str = "memory_usage_percent";

    /// Input event latency histogram (milliseconds)
    pub const INPUT_LATENCY_MS: &str = "input_latency_ms";
    /// Video encoding/transmission latency histogram (milliseconds)
    pub const VIDEO_LATENCY_MS: &str = "video_latency_ms";
    /// End-to-end latency histogram (milliseconds)
    pub const END_TO_END_LATENCY_MS: &str = "end_to_end_latency_ms";

    // -- EGFX pipeline metrics --

    /// EGFX frame acknowledgement queue depth (gauge)
    pub const EGFX_QUEUE_DEPTH: &str = "egfx_queue_depth";
    /// Total EGFX frame acknowledgements received (counter)
    pub const EGFX_FRAME_ACKS: &str = "egfx_frame_acks_total";
    /// Client-reported decode+render time in microseconds (histogram, from QoE)
    pub const CLIENT_DECODE_RENDER_US: &str = "client_decode_render_us";

    // -- Encoder metrics --

    /// Current encoder FPS (gauge)
    pub const ENCODER_FPS: &str = "encoder_fps";
    /// Current encoder bitrate in kbps (gauge)
    pub const ENCODER_BITRATE_KBPS: &str = "encoder_bitrate_kbps";
    /// Encoding duration per frame in milliseconds (histogram)
    pub const ENCODE_DURATION_MS: &str = "encode_duration_ms";
    /// Total frames encoded (counter)
    pub const FRAMES_ENCODED: &str = "frames_encoded_total";
}

const NAMESPACE: &str = "lamco_rdp_";

/// Add `lamco_rdp_` prefix if not already present
fn namespace_metric(name: &str) -> String {
    if name.starts_with(NAMESPACE) {
        name.to_string()
    } else {
        format!("{NAMESPACE}{name}")
    }
}

/// Derive a HELP string from the metric name.
/// Uses the known metric_names constants for accurate descriptions,
/// falls back to humanizing the metric name.
fn metric_help(name: &str) -> &'static str {
    match name {
        metric_names::FRAMES_RECEIVED => "Total video frames received from PipeWire.",
        metric_names::FRAMES_PROCESSED => "Total video frames successfully processed.",
        metric_names::FRAMES_DROPPED => "Total video frames dropped.",
        metric_names::FRAME_PROCESSING_TIME_MS => "Frame processing time in milliseconds.",
        metric_names::CONVERSIONS_TOTAL => "Total format conversions performed.",
        metric_names::CONVERSION_TIME_MS => "Conversion time in milliseconds.",
        metric_names::FRAMES_DISPATCHED => "Total frames dispatched to RDP clients.",
        metric_names::FRAMES_QUEUED => "Current frames waiting in dispatch queue.",
        metric_names::BYTES_SENT => "Total bytes sent to RDP clients.",
        metric_names::BYTES_RECEIVED => "Total bytes received from RDP clients.",
        metric_names::CONNECTIONS_ACTIVE => "Currently active RDP connections.",
        metric_names::CONNECTIONS_TOTAL => "Total RDP connections since server start.",
        metric_names::CONNECTION_ERRORS => "Total connection errors.",
        metric_names::EGFX_QUEUE_DEPTH => "EGFX frame acknowledgement queue depth.",
        metric_names::EGFX_FRAME_ACKS => "Total EGFX frame acknowledgements received.",
        metric_names::CLIENT_DECODE_RENDER_US => {
            "Client-reported decode+render time in microseconds."
        }
        metric_names::ENCODER_FPS => "Current encoder frames per second.",
        metric_names::ENCODER_BITRATE_KBPS => "Current encoder bitrate in kbps.",
        metric_names::ENCODE_DURATION_MS => "Encoding duration per frame in milliseconds.",
        _ => "Server metric.",
    }
}

/// Timer helper for measuring durations
pub struct Timer {
    start: Instant,
}

impl Timer {
    /// Start a new timer
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// Get elapsed time in milliseconds
    pub fn elapsed_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// Get elapsed time in microseconds
    pub fn elapsed_us(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1_000_000.0
    }

    /// Get elapsed time in nanoseconds
    pub fn elapsed_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counter() {
        let metrics = MetricsCollector::new();

        metrics.increment_counter("test_counter", 1);
        assert_eq!(metrics.get_counter("test_counter"), Some(1));

        metrics.increment_counter("test_counter", 5);
        assert_eq!(metrics.get_counter("test_counter"), Some(6));
    }

    #[test]
    fn test_gauge() {
        let metrics = MetricsCollector::new();

        metrics.set_gauge("test_gauge", 42.5);
        assert_eq!(metrics.get_gauge("test_gauge"), Some(42.5));

        metrics.set_gauge("test_gauge", 100.0);
        assert_eq!(metrics.get_gauge("test_gauge"), Some(100.0));
    }

    #[test]
    fn test_histogram() {
        let metrics = MetricsCollector::new();

        metrics.record_histogram("test_histogram", 10.0);
        metrics.record_histogram("test_histogram", 20.0);
        metrics.record_histogram("test_histogram", 30.0);

        let stats = metrics.get_histogram("test_histogram").unwrap();
        assert_eq!(stats.count, 3);
        assert!((stats.min - 10.0).abs() < 0.01);
        assert!((stats.max - 30.0).abs() < 0.01);
        assert!((stats.mean - 20.0).abs() < 0.01);
    }

    #[test]
    fn test_snapshot() {
        let metrics = MetricsCollector::new();

        metrics.increment_counter("counter1", 10);
        metrics.set_gauge("gauge1", 42.0);
        metrics.record_histogram("hist1", 5.0);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.counters.get("counter1"), Some(&10));
        assert_eq!(snapshot.gauges.get("gauge1"), Some(&42.0));
        assert!(snapshot.histograms.contains_key("hist1"));
    }

    #[test]
    fn test_reset() {
        let metrics = MetricsCollector::new();

        metrics.increment_counter("test", 1);
        metrics.set_gauge("test", 1.0);
        metrics.record_histogram("test", 1.0);

        metrics.reset();

        assert_eq!(metrics.get_counter("test"), None);
        assert_eq!(metrics.get_gauge("test"), None);
        assert_eq!(metrics.get_histogram("test"), None);
    }

    #[test]
    fn test_prometheus_export() {
        let metrics = MetricsCollector::new();

        metrics.increment_counter("test_counter", 42);
        metrics.set_gauge("test_gauge", 3.14);

        let output = metrics.export_prometheus();
        // Prometheus export adds lamco_rdp_ namespace prefix
        assert!(output.contains("lamco_rdp_test_counter 42"));
        assert!(output.contains("lamco_rdp_test_gauge 3.14"));
        // HELP and TYPE metadata must be present
        assert!(output.contains("# HELP lamco_rdp_test_counter"));
        assert!(output.contains("# TYPE lamco_rdp_test_counter counter"));
        assert!(output.contains("# TYPE lamco_rdp_test_gauge gauge"));
        // Uptime gauge is always present
        assert!(output.contains("lamco_rdp_uptime_seconds"));
    }

    #[test]
    fn test_json_export() {
        let metrics = MetricsCollector::new();

        metrics.increment_counter("test", 1);
        let json = metrics.export_json().unwrap();
        assert!(json.contains("\"test\""));
    }

    #[test]
    fn test_timer() {
        let timer = Timer::new();
        std::thread::sleep(Duration::from_millis(10));

        let elapsed = timer.elapsed_ms();
        assert!(elapsed >= 10.0);
        assert!(elapsed < 50.0); // Allow some overhead
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "integer-valued test data has exact float representation"
    )]
    fn test_percentile() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];

        assert_eq!(percentile(&values, 0.50), 5.0);
        assert_eq!(percentile(&values, 0.95), 9.0);
        assert_eq!(percentile(&values, 0.99), 9.0);
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "integer-valued test data has exact float representation"
    )]
    fn test_histogram_stats() {
        let mut hist = Histogram::new();

        hist.record(10.0);
        hist.record(20.0);
        hist.record(30.0);
        hist.record(40.0);
        hist.record(50.0);

        let stats = hist.stats();
        assert_eq!(stats.count, 5);
        assert_eq!(stats.sum, 150.0);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 50.0);
        assert_eq!(stats.mean, 30.0);
        assert_eq!(stats.p50, 30.0);
    }

    #[test]
    fn test_histogram_rolling_window() {
        let mut hist = Histogram::new();

        // Fill beyond MAX_HISTOGRAM_SAMPLES
        for i in 0..MAX_HISTOGRAM_SAMPLES + 500 {
            hist.record(i as f64);
        }

        // Window should be capped
        assert_eq!(hist.values.len(), MAX_HISTOGRAM_SAMPLES);

        let stats = hist.stats();
        // total_count tracks all recordings, not just window
        assert_eq!(stats.count, (MAX_HISTOGRAM_SAMPLES + 500) as u64);
        // Window mean should reflect the most recent 10k samples (500..10500)
        let expected_window_mean = (500.0 + (MAX_HISTOGRAM_SAMPLES + 499) as f64) / 2.0;
        assert!((stats.mean - expected_window_mean).abs() < 1.0);
    }
}
