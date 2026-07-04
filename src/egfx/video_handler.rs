//! EGFX Video Handler
//!
//! Bridges raw framebuffer data to the EGFX H.264 encoding pipeline. The frame
//! source is pluggable via [`RawFrame`]: the QEMU console feeds it from the
//! D-Bus SharedFramebuffer, and any future desktop path can feed it from a
//! PipeWire capture without a second copy of this handler.

use std::{sync::Arc, time::Instant};

#[cfg(feature = "h264")]
use tokio::sync::Mutex;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, trace, warn};

#[cfg(feature = "h264")]
use crate::egfx::encoder::{Avc420Encoder, EncoderConfig};
use crate::egfx::encoder::{EncoderError, EncoderResult};

/// Source-agnostic raw frame — produced by QEMU D-Bus framebuffer or any capture backend.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Configuration for EGFX video handling
#[derive(Debug, Clone)]
pub struct EgfxVideoConfig {
    pub bitrate_kbps: u32,
    pub max_fps: u32,
    pub enable_frame_skip: bool,
    pub quality_preset: u32,
    pub avc_444_mode: bool,
}

impl Default for EgfxVideoConfig {
    fn default() -> Self {
        Self {
            bitrate_kbps: 5000,
            max_fps: 30,
            enable_frame_skip: true,
            quality_preset: 50,
            avc_444_mode: false,
        }
    }
}

/// Encoded frame ready for EGFX transmission
#[derive(Debug)]
pub struct EncodedFrame {
    pub h264_data: Vec<u8>,
    pub timestamp_ms: u64,
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
    pub encode_time_us: u64,
}

/// Statistics for video encoding
#[derive(Debug, Default, Clone)]
pub struct EncodingStats {
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub keyframes: u64,
    pub total_bytes: u64,
    pub avg_encode_time_us: u64,
    pub peak_encode_time_us: u64,
}

/// EGFX Video Handler
///
/// Receives raw frames and produces H.264 encoded data for EGFX transmission.
pub struct EgfxVideoHandler {
    #[expect(dead_code, reason = "used for runtime reconfiguration")]
    config: EgfxVideoConfig,

    #[cfg(feature = "h264")]
    encoder: Mutex<Avc420Encoder>,

    stats: RwLock<EncodingStats>,
    session_start: Instant,
    output_tx: mpsc::Sender<EncodedFrame>,
    surface_size: RwLock<(u32, u32)>,
    frame_counter: std::sync::atomic::AtomicU64,
}

impl EgfxVideoHandler {
    #[cfg(feature = "h264")]
    pub fn new(
        config: EgfxVideoConfig,
        initial_width: u32,
        initial_height: u32,
        output_tx: mpsc::Sender<EncodedFrame>,
    ) -> EncoderResult<Self> {
        let encoder_config = EncoderConfig {
            bitrate_kbps: config.bitrate_kbps,
            max_fps: config.max_fps as f32,
            enable_skip_frame: config.enable_frame_skip,
            width: Some(initial_width as u16),
            height: Some(initial_height as u16),
            color_space: None,
            ..Default::default()
        };

        let encoder = Avc420Encoder::new(encoder_config)?;

        Ok(Self {
            config,
            encoder: Mutex::new(encoder),
            stats: RwLock::new(EncodingStats::default()),
            session_start: Instant::now(),
            output_tx,
            surface_size: RwLock::new((initial_width, initial_height)),
            frame_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    #[cfg(not(feature = "h264"))]
    pub fn new(
        _config: EgfxVideoConfig,
        _initial_width: u32,
        _initial_height: u32,
        _output_tx: mpsc::Sender<EncodedFrame>,
    ) -> EncoderResult<Self> {
        Err(EncoderError::InitFailed(
            "H.264 support not compiled in (enable 'h264' feature)".to_string(),
        ))
    }

    #[cfg(feature = "h264")]
    pub async fn process_frame(&self, frame: RawFrame) -> EncoderResult<bool> {
        let frame_num = self
            .frame_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let timestamp_ms = self.session_start.elapsed().as_millis() as u64;

        let (current_w, current_h) = *self.surface_size.read().await;
        if frame.width != current_w || frame.height != current_h {
            info!(
                "Surface resize: {}x{} -> {}x{}",
                current_w, current_h, frame.width, frame.height
            );
            *self.surface_size.write().await = (frame.width, frame.height);
        }

        let encode_start = Instant::now();

        let encoded = {
            let mut encoder = self.encoder.lock().await;
            encoder.encode_bgra(&frame.data, frame.width, frame.height, timestamp_ms)?
        };

        let encode_time_us = encode_start.elapsed().as_micros() as u64;

        {
            let mut stats = self.stats.write().await;
            stats.frames_encoded += 1;
            stats.peak_encode_time_us = stats.peak_encode_time_us.max(encode_time_us);

            let n = stats.frames_encoded as f64;
            stats.avg_encode_time_us =
                ((stats.avg_encode_time_us as f64 * (n - 1.0) + encode_time_us as f64) / n) as u64;
        }

        let h264_frame = match encoded {
            Some(f) => f,
            None => {
                trace!("Frame {} skipped by encoder", frame_num);
                return Ok(false);
            }
        };

        {
            let mut stats = self.stats.write().await;
            if h264_frame.is_keyframe {
                stats.keyframes += 1;
            }
            stats.total_bytes += h264_frame.data.len() as u64;
        }

        let encoded_frame = EncodedFrame {
            h264_data: h264_frame.data,
            timestamp_ms,
            is_keyframe: h264_frame.is_keyframe,
            width: frame.width,
            height: frame.height,
            encode_time_us,
        };

        match self.output_tx.try_send(encoded_frame) {
            Ok(()) => {
                if frame_num.is_multiple_of(30) {
                    debug!(
                        "Encoded frame {} in {}us, keyframe={}",
                        frame_num, encode_time_us, h264_frame.is_keyframe
                    );
                }
                Ok(true)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                let mut stats = self.stats.write().await;
                stats.frames_dropped += 1;
                warn!("EGFX output queue full - dropping frame {}", frame_num);
                Ok(false)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("EGFX output channel closed");
                Err(EncoderError::EncodeFailed(
                    "Output channel closed".to_string(),
                ))
            }
        }
    }

    #[cfg(not(feature = "h264"))]
    pub async fn process_frame(&self, _frame: RawFrame) -> EncoderResult<bool> {
        Err(EncoderError::InitFailed(
            "H.264 support not compiled in".to_string(),
        ))
    }

    #[cfg(feature = "h264")]
    pub async fn force_keyframe(&self) {
        let mut encoder = self.encoder.lock().await;
        encoder.force_keyframe();
        debug!("Keyframe requested");
    }

    #[cfg(not(feature = "h264"))]
    pub async fn force_keyframe(&self) {}

    pub async fn get_stats(&self) -> EncodingStats {
        self.stats.read().await.clone()
    }

    pub async fn reset_stats(&self) {
        *self.stats.write().await = EncodingStats::default();
    }

    pub async fn surface_size(&self) -> (u32, u32) {
        *self.surface_size.read().await
    }
}

pub(super) struct EgfxVideoHandlerFactory {
    config: EgfxVideoConfig,
}

#[expect(
    dead_code,
    reason = "factory pattern for multi-monitor video handler creation"
)]
impl EgfxVideoHandlerFactory {
    pub(super) fn new(config: EgfxVideoConfig) -> Self {
        Self { config }
    }

    pub(super) fn build_handler(
        &self,
        #[cfg_attr(not(feature = "h264"), allow(unused_variables))] width: u32,
        #[cfg_attr(not(feature = "h264"), allow(unused_variables))] height: u32,
    ) -> Option<mpsc::Receiver<EncodedFrame>> {
        #[cfg(feature = "h264")]
        {
            let (tx, rx) = mpsc::channel(16);

            match EgfxVideoHandler::new(self.config.clone(), width, height, tx) {
                Ok(_handler) => {
                    info!(
                        "EGFX video handler created: {}x{}, {}kbps",
                        width, height, self.config.bitrate_kbps
                    );
                    Some(rx)
                }
                Err(e) => {
                    error!("Failed to create EGFX video handler: {:?}", e);
                    None
                }
            }
        }

        #[cfg(not(feature = "h264"))]
        {
            warn!("EGFX video handler unavailable: H.264 feature not enabled");
            None
        }
    }

    pub(super) fn is_available(&self) -> bool {
        cfg!(feature = "h264")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_egfx_video_config_default() {
        let config = EgfxVideoConfig::default();
        assert_eq!(config.bitrate_kbps, 5000);
        assert_eq!(config.max_fps, 30);
        assert!(config.enable_frame_skip);
    }

    #[test]
    fn test_factory_availability() {
        let factory = EgfxVideoHandlerFactory::new(EgfxVideoConfig::default());
        #[cfg(feature = "h264")]
        assert!(factory.is_available());
        #[cfg(not(feature = "h264"))]
        assert!(!factory.is_available());
    }

    #[cfg(feature = "h264")]
    #[tokio::test]
    async fn test_handler_creation() {
        let (tx, _rx) = mpsc::channel(16);
        let config = EgfxVideoConfig::default();

        let handler = EgfxVideoHandler::new(config, 1920, 1080, tx);

        if let Ok(handler) = handler {
            let size = handler.surface_size().await;
            assert_eq!(size, (1920, 1080));
        }
    }

    #[tokio::test]
    async fn test_stats_tracking() {
        let stats = EncodingStats::default();
        assert_eq!(stats.frames_encoded, 0);
        assert_eq!(stats.frames_dropped, 0);
    }
}
