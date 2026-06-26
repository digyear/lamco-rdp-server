//! RDP6 Planar Codec Encoder
//!
//! Encodes BGRX32 bitmaps to RDP6_BITMAP_STREAM format for EGFX WireToSurface1 PDU.
//! Used when the client supports Codec1Type::Planar (0xa) but NOT RemoteFX (0x3).
//!
//! Format: formatHeader(0x30) + RLE-compressed R plane + G plane + B plane
//! Compression: Delta encoding (zig-zag) + per-scanline RLE
//! Typical ratio: 5:1 to 25:1 for desktop content (lossless)

/// RDP6 Planar encoder. Reuse across frames for buffer efficiency.
pub struct PlanarEncoder {
    // Working buffers
    planes: [Vec<u8>; 4],       // [A, R, G, B] separated planes
    delta_planes: [Vec<u8>; 4], // Delta-encoded planes
}

impl PlanarEncoder {
    pub fn new(width: usize, height: usize) -> Self {
        let plane_size = width * height;
        Self {
            planes: [
                vec![0u8; plane_size],
                vec![0u8; plane_size],
                vec![0u8; plane_size],
                vec![0u8; plane_size],
            ],
            delta_planes: [
                vec![0u8; plane_size],
                vec![0u8; plane_size],
                vec![0u8; plane_size],
                vec![0u8; plane_size],
            ],
        }
    }

    /// Encode a BGRX/BGRA32 bitmap to RDP6 Planar format.
    ///
    /// - `bgra_data`: raw BGRX32 or BGRA32 pixels, **top-down** order
    ///   (row 0 = top of image, matching IronRDP BitmapUpdate convention)
    /// - `width`, `height`: image dimensions
    /// - `scanline`: bytes per row in the input (0 = width * 4)
    ///
    /// Returns RDP6_BITMAP_STREAM bytes: formatHeader + RLE(R) + RLE(G) + RLE(B)
    pub fn encode(
        &mut self,
        bgra_data: &[u8],
        width: usize,
        height: usize,
        scanline: usize,
    ) -> Vec<u8> {
        let scanline = if scanline == 0 { width * 4 } else { scanline };
        let plane_size = width * height;

        // Ensure buffers are large enough
        for i in 0..4 {
            if self.planes[i].len() < plane_size {
                self.planes[i] = vec![0u8; plane_size];
                self.delta_planes[i] = vec![0u8; plane_size];
            }
        }

        // Step 1: Split BGRX into planes [A, R, G, B]
        // Input is top-down (row 0 = top of image), matching IronRDP BitmapUpdate.
        // RDP6_BITMAP_STREAM is also top-down per MS-RDPEGDI 2.2.2.5.1.
        for y in 0..height {
            let row_offset = y * scanline;
            for x in 0..width {
                let off = row_offset + x * 4;
                let b = bgra_data[off];
                let g = bgra_data[off + 1];
                let r = bgra_data[off + 2];
                let a = bgra_data[off + 3];
                let k = y * width + x;
                self.planes[0][k] = a;
                self.planes[1][k] = r;
                self.planes[2][k] = g;
                self.planes[3][k] = b;
            }
        }

        // Step 2: Delta encode each plane into delta_planes.
        // Row 0 stays as-is. Row N = zig_zag(row[N] - row[N-1]).
        // Process rows bottom-to-top to avoid overwriting row N-1 before use.
        for p in 0..4 {
            self.delta_planes[p][..width].copy_from_slice(&self.planes[p][..width]);
            for y in (1..height).rev() {
                let row_start = y * width;
                let prev_start = (y - 1) * width;
                for x in 0..width {
                    let delta = self.planes[p][row_start + x] as i16
                        - self.planes[p][prev_start + x] as i16;
                    self.delta_planes[p][row_start + x] = zig_zag_encode(delta);
                }
            }
        }

        // Step 3: RLE compress R, G, B planes (skip alpha — NA=1 in formatHeader)
        let rle_r = rle_compress_plane(&self.delta_planes[1], width, height);
        let rle_g = rle_compress_plane(&self.delta_planes[2], width, height);
        let rle_b = rle_compress_plane(&self.delta_planes[3], width, height);

        // Step 4: Assemble RDP6_BITMAP_STREAM
        // formatHeader (MS-RDPEGDI 2.2.2.5.1 / IronRDP BitmapStreamHeader):
        //   bits 2:0 = color_loss_level (0 = ARGB, no color conversion)
        //   bit  3   = chroma_subsampling (0 = no subsampling)
        //   bit  4   = enable_rle_compression (1 = RLE enabled)
        //   bit  5   = NA (1 = no alpha plane; use_alpha = (NA==0))
        // 0x30 = 0b00110000: RLE=1, NA=1 → 3 planes (R, G, B), no alpha
        let mut output = Vec::with_capacity(1 + rle_r.len() + rle_g.len() + rle_b.len());
        output.push(0x30);
        output.extend_from_slice(&rle_r);
        output.extend_from_slice(&rle_g);
        output.extend_from_slice(&rle_b);
        output
    }
}

/// Zig-zag encode a signed row delta into an unsigned byte.
/// Maps: 0→0, 1→2, -1→1, 2→4, -2→3, ...
/// Matches IronRDP's RleEncoderScanlineIterator::delta_value() behavior.
#[inline]
fn zig_zag_encode(delta: i16) -> u8 {
    // Truncate to i8 range (wraps at ±128, matching FreeRDP / MS spec)
    let s = delta as i8;
    if s >= 0 {
        (s as u8) << 1
    } else {
        ((-s) as u8) * 2 - 1
    }
}

/// RLE-compress a delta-encoded plane. Each scanline is compressed independently.
fn rle_compress_plane(plane: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(width * height / 2);
    for y in 0..height {
        let row = &plane[y * width..(y + 1) * width];
        encode_rle_scanline(row, &mut output);
    }
    output
}

/// RLE-encode a single scanline per MS-RDPEGDI 3.1.9.2.
///
/// This mirrors IronRDP's RDP6 plane encoder. A segment carries up to 15 raw
/// bytes followed by a run of the last raw byte. Long runs use the special
/// 16/32+ forms. Some clients, including Microsoft RD Client on Android, are
/// sensitive to this canonical segmentation even when a non-canonical stream
/// round-trips through IronRDP's decoder.
fn encode_rle_scanline(input: &[u8], output: &mut Vec<u8>) {
    if input.is_empty() {
        return;
    }

    let mut iter = input.iter().copied();
    let first = iter.next().expect("input is not empty");
    let mut raw = vec![first];
    // `count` is the number of repeats after the first byte in the current run.
    let mut seq = (first, 0usize);

    for byte in iter {
        let (last, count) = seq;
        seq = if byte == last {
            (byte, count + 1)
        } else {
            match count {
                3.. => {
                    encode_segment(&raw, count, output);
                    raw.clear();
                }
                2 => raw.extend_from_slice(&[last, last]),
                1 => raw.push(last),
                _ => {}
            }
            raw.push(byte);
            (byte, 0)
        };
    }

    let (last, mut count) = seq;
    if count < 3 {
        raw.extend(std::iter::repeat_n(last, count));
        count = 0;
    }
    encode_segment(&raw, count, output);
}

fn encode_segment(mut raw: &[u8], run: usize, output: &mut Vec<u8>) {
    if raw.is_empty() {
        return;
    }

    while raw.len() > 15 {
        encode_segment(&raw[..15], 0, output);
        raw = &raw[15..];
    }

    let raw_len = raw.len() as u8;
    let run_capped = run.min(15) as u8;
    output.push((raw_len << 4) | run_capped);
    output.extend_from_slice(raw);

    if run > 15 {
        let last = *raw.last().expect("raw segment is not empty");
        encode_long_sequence(run - 15, last, output);
    }
}

fn encode_long_sequence(mut run: usize, last: u8, output: &mut Vec<u8>) {
    while run >= 16 {
        let current = run.min(47);
        let c_raw_bytes = (current / 16).min(2);
        let n_run_length = current - c_raw_bytes * 16;
        output.push(((n_run_length as u8) << 4) | c_raw_bytes as u8);
        run -= current;
    }

    if run > 0 {
        match run {
            short @ 1..=3 => encode_segment(&vec![last; short], 0, output),
            long => encode_segment(&[last], long - 1, output),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zig_zag_encode() {
        assert_eq!(zig_zag_encode(0), 0);
        assert_eq!(zig_zag_encode(1), 2);
        assert_eq!(zig_zag_encode(-1), 1);
        assert_eq!(zig_zag_encode(2), 4);
        assert_eq!(zig_zag_encode(-2), 3);
        assert_eq!(zig_zag_encode(127), 254);
        assert_eq!(zig_zag_encode(-127), 253);
    }

    #[test]
    fn test_solid_color() {
        let w = 4;
        let h = 2;
        let mut bgra = vec![0u8; w * h * 4];
        for px in bgra.chunks_exact_mut(4) {
            px[0] = 0xFF; // B
            px[1] = 0x00; // G
            px[2] = 0x00; // R
            px[3] = 0xFF; // X
        }

        let mut enc = PlanarEncoder::new(w, h);
        let out = enc.encode(&bgra, w, h, 0);

        assert_eq!(out[0], 0x30);
        assert!(out.len() < w * h * 3);
    }

    #[test]
    fn test_gradient() {
        let w = 64;
        let h = 64;
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                bgra[idx] = (x * 4) as u8;
                bgra[idx + 1] = (y * 4) as u8;
                bgra[idx + 2] = 0;
                bgra[idx + 3] = 0xFF;
            }
        }

        let mut enc = PlanarEncoder::new(w, h);
        let out = enc.encode(&bgra, w, h, 0);

        assert_eq!(out[0], 0x30);
        assert!(out.len() < w * h * 3);
    }

    /// Roundtrip: encode BGRX (top-down) → Planar → decode with IronRDP → compare RGB.
    #[test]
    fn test_roundtrip_ironrdp_decode_solid() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        let w = 4usize;
        let h = 2usize;
        let mut bgra = vec![0u8; w * h * 4];
        for px in bgra.chunks_exact_mut(4) {
            px[0] = 0x00; // B
            px[1] = 0x55; // G
            px[2] = 0xAA; // R
            px[3] = 0xFF; // X
        }

        let mut enc = PlanarEncoder::new(w, h);
        let encoded = enc.encode(&bgra, w, h, 0);

        assert_eq!(encoded[0], 0x30);

        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb_out = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h)
            .expect("IronRDP failed to decode our Planar output");

        assert_eq!(rgb_out.len(), w * h * 3);
        for (i, chunk) in rgb_out.chunks_exact(3).enumerate() {
            assert_eq!(chunk[0], 0xAA, "R mismatch at pixel {i}");
            assert_eq!(chunk[1], 0x55, "G mismatch at pixel {i}");
            assert_eq!(chunk[2], 0x00, "B mismatch at pixel {i}");
        }
    }

    /// Roundtrip with gradient pattern (tests delta encoding correctness).
    #[test]
    fn test_roundtrip_ironrdp_decode_gradient() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        let w = 8usize;
        let h = 4usize;
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) * 4;
                bgra[idx] = (x * 16) as u8; // B
                bgra[idx + 1] = (y * 32) as u8; // G
                bgra[idx + 2] = ((x + y) * 8) as u8; // R
                bgra[idx + 3] = 0xFF; // X
            }
        }

        let mut enc = PlanarEncoder::new(w, h);
        let encoded = enc.encode(&bgra, w, h, 0);

        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb_out = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h)
            .expect("IronRDP failed to decode gradient Planar output");

        assert_eq!(rgb_out.len(), w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * 4;
                let dst = (y * w + x) * 3;
                assert_eq!(rgb_out[dst], bgra[src + 2], "R mismatch at ({x},{y})");
                assert_eq!(rgb_out[dst + 1], bgra[src + 1], "G mismatch at ({x},{y})");
                assert_eq!(rgb_out[dst + 2], bgra[src], "B mismatch at ({x},{y})");
            }
        }
    }

    /// Roundtrip: all-zero frame (stress test for zero-byte handling).
    #[test]
    fn test_roundtrip_ironrdp_decode_black_frame() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        let w = 32usize;
        let h = 8usize;
        let bgra = vec![0u8; w * h * 4];

        let mut enc = PlanarEncoder::new(w, h);
        let encoded = enc.encode(&bgra, w, h, 0);

        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb_out = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h)
            .expect("IronRDP failed to decode all-zero Planar output");

        assert_eq!(rgb_out.len(), w * h * 3);
        for (i, &v) in rgb_out.iter().enumerate() {
            assert_eq!(v, 0, "non-zero at index {i}");
        }
    }

    /// Roundtrip: random-ish pixel pattern to stress RLE with mixed runs/raws.
    #[test]
    fn test_roundtrip_ironrdp_decode_mixed_pattern() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        let w = 16usize;
        let h = 8usize;
        let mut bgra = vec![0u8; w * h * 4];
        for (i, chunk) in bgra.chunks_exact_mut(4).enumerate() {
            // Alternating pattern + some runs
            let v = if i % 3 == 0 {
                0x80
            } else if i % 5 == 0 {
                0xFF
            } else {
                i as u8
            };
            chunk[0] = v; // B
            chunk[1] = v / 2; // G
            chunk[2] = v ^ 0x55; // R
            chunk[3] = 0xFF; // X
        }

        let mut enc = PlanarEncoder::new(w, h);
        let encoded = enc.encode(&bgra, w, h, 0);

        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb_out = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h)
            .expect("IronRDP failed to decode mixed pattern");

        assert_eq!(rgb_out.len(), w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * 4;
                let dst = (y * w + x) * 3;
                assert_eq!(rgb_out[dst], bgra[src + 2], "R mismatch at ({x},{y})");
                assert_eq!(rgb_out[dst + 1], bgra[src + 1], "G mismatch at ({x},{y})");
                assert_eq!(rgb_out[dst + 2], bgra[src], "B mismatch at ({x},{y})");
            }
        }
    }

    /// Roundtrip: 1920x1080 solid color (simulates real desktop frame).
    #[test]
    fn test_roundtrip_fullhd_solid() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        let w = 1920usize;
        let h = 1080usize;
        let mut bgra = vec![0u8; w * h * 4];
        for chunk in bgra.chunks_exact_mut(4) {
            chunk[0] = 0x1A; // B
            chunk[1] = 0x2B; // G
            chunk[2] = 0x3C; // R
            chunk[3] = 0xFF; // X
        }

        let mut enc = PlanarEncoder::new(w, h);
        let encoded = enc.encode(&bgra, w, h, 0);

        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb_out = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h)
            .expect("IronRDP failed to decode 1920x1080 Planar output");

        assert_eq!(rgb_out.len(), w * h * 3);
        for (i, chunk) in rgb_out.chunks_exact(3).enumerate() {
            assert_eq!(chunk[0], 0x3C, "R mismatch at pixel {i}");
            assert_eq!(chunk[1], 0x2B, "G mismatch at pixel {i}");
            assert_eq!(chunk[2], 0x1A, "B mismatch at pixel {i}");
        }
    }

    /// Edge case: run lengths that leave remainder 1 or 2 after 32/16 decomposition.
    /// These previously caused SegmentDoNotFitScanline (0x200d crash root cause).
    #[test]
    fn test_roundtrip_run_remainder_edge_cases() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        // Run lengths that have remainder 1 or 2 after decomposing into 32+16+n:
        // e.g., 33 = 32+1, 34 = 32+2, 49 = 32+16+1, 50 = 32+16+2, 835 = 26*32+2+1, etc.
        for run_len in [33usize, 34, 49, 50, 51, 52, 835, 946, 947, 1919, 1920] {
            let w = run_len;
            let h = 1usize;
            // Solid color row: all pixels have R=0x42, which after delta encoding
            // gives row 0 = [0x42, ...] (no delta needed)
            let mut bgra = vec![0u8; w * h * 4];
            for px in bgra.chunks_exact_mut(4) {
                px[0] = 0x10; // B
                px[1] = 0x20; // G
                px[2] = 0x42; // R
                px[3] = 0xFF; // X
            }

            let mut enc = PlanarEncoder::new(w, h);
            let encoded = enc.encode(&bgra, w, h, 0);

            let mut decoder = BitmapStreamDecoder::default();
            let mut rgb_out = Vec::new();
            match decoder.decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h) {
                Ok(()) => {
                    assert_eq!(rgb_out.len(), w * h * 3, "size mismatch for run={run_len}");
                    for (i, chunk) in rgb_out.chunks_exact(3).enumerate() {
                        assert_eq!(chunk[0], 0x42, "R mismatch at pixel {i} for run={run_len}");
                        assert_eq!(chunk[1], 0x20, "G mismatch at pixel {i} for run={run_len}");
                        assert_eq!(chunk[2], 0x10, "B mismatch at pixel {i} for run={run_len}");
                    }
                }
                Err(e) => panic!("IronRDP decode failed for run={run_len}: {e:?}"),
            }
        }
    }

    /// Regression test: 1920-wide row where delta=0 run=835 caused col overflow.
    #[test]
    fn test_roundtrip_1920_width_run835_regression() {
        use ironrdp_graphics::rdp6::BitmapStreamDecoder;

        // Reproduce the exact failure: 1920 width, row where R plane has
        // a run of 835 zeros followed by non-zero pixels.
        let w = 1920usize;
        let h = 2usize;
        let mut bgra = vec![0u8; w * h * 4];
        // Row 0: first 835 pixels R=0x50, rest R=0x60
        for x in 0..w {
            let r = if x < 835 { 0x50u8 } else { 0x60 };
            bgra[x * 4 + 2] = r;
            bgra[x * 4 + 3] = 0xFF;
        }
        // Row 1: same as row 0 (delta = 0 for all pixels = run=1920)
        for x in 0..w {
            let r = if x < 835 { 0x50u8 } else { 0x60 };
            bgra[w * 4 + x * 4 + 2] = r;
            bgra[w * 4 + x * 4 + 3] = 0xFF;
        }

        let mut enc = PlanarEncoder::new(w, h);
        let encoded = enc.encode(&bgra, w, h, 0);

        let mut decoder = BitmapStreamDecoder::default();
        let mut rgb_out = Vec::new();
        decoder
            .decode_bitmap_stream_to_rgb24(&encoded, &mut rgb_out, w, h)
            .expect("1920-width run835 regression test failed");

        assert_eq!(rgb_out.len(), w * h * 3);
    }
}
