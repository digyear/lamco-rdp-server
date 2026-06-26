//! Standalone tool to verify a dumped Planar bitmap stream file.
//!
//! Usage: verify_planar <file.bin> <width> <height>
//!
//! Reads a raw RDP6_BITMAP_STREAM file and decodes it with IronRDP,
//! printing the result for offline debugging of 0x200d crashes.

use ironrdp_graphics::rdp6::BitmapStreamDecoder;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("Usage: {} <file.bin> <width> <height>", args[0]);
        std::process::exit(1);
    }

    let path = &args[1];
    let width: usize = args[2].parse().expect("width must be integer");
    let height: usize = args[3].parse().expect("height must be integer");

    let data = std::fs::read(path).expect("cannot read file");
    println!("File: {} ({} bytes)", path, data.len());
    println!("Dimensions: {}x{}", width, height);

    if data.is_empty() {
        eprintln!("ERROR: empty file");
        std::process::exit(1);
    }

    // Print header byte analysis
    let header = data[0];
    let color_loss_level = header & 0x07;
    let chroma_subsampling = (header & 0x08) != 0;
    let rle_enabled = (header & 0x10) != 0;
    let no_alpha = (header & 0x20) != 0;
    println!("\nBitmapStreamHeader (0x{:02x}):", header);
    println!("  color_loss_level: {} (0=ARGB)", color_loss_level);
    println!("  chroma_subsampling: {}", chroma_subsampling);
    println!("  rle_enabled: {}", rle_enabled);
    println!("  use_alpha: {} (NA bit={})", !no_alpha, no_alpha as u8);

    let plane_count = if no_alpha { 3 } else { 4 };
    println!("  → {} color planes expected", plane_count);
    println!(
        "  → Expected decoded size: {} bytes ({}x{}x3)",
        width * height * 3,
        width,
        height
    );

    // Print first 128 bytes as hex
    let preview_len = data.len().min(128);
    print!("\nFirst {} bytes: ", preview_len);
    for (i, b) in data[..preview_len].iter().enumerate() {
        if i > 0 && i % 16 == 0 {
            print!("\n              ");
        }
        print!("{:02x} ", b);
    }
    println!();

    // Try to decode with IronRDP
    println!("\nDecoding with IronRDP BitmapStreamDecoder...");
    let mut decoder = BitmapStreamDecoder::default();
    let mut rgb_out = Vec::new();

    match decoder.decode_bitmap_stream_to_rgb24(&data, &mut rgb_out, width, height) {
        Ok(()) => {
            println!("✅ SUCCESS: decoded {} bytes of RGB24 data", rgb_out.len());
            println!("Expected:  {} bytes", width * height * 3);

            if rgb_out.len() == width * height * 3 {
                println!("✅ Size matches perfectly");
            } else {
                println!("⚠️  Size MISMATCH");
            }

            // Print first few pixels
            println!("\nFirst 8 pixels (R,G,B):");
            for (i, chunk) in rgb_out.chunks_exact(3).take(8).enumerate() {
                println!("  pixel[{i}]: R={} G={} B={}", chunk[0], chunk[1], chunk[2]);
            }

            // Check for corruption (all same value = possibly wrong)
            let all_same = rgb_out.windows(2).all(|w| w[0] == w[1]);
            if all_same {
                println!(
                    "⚠️  WARNING: all decoded bytes are identical ({}) — possibly encoder bug",
                    rgb_out[0]
                );
            }
        }
        Err(e) => {
            println!("❌ FAILED to decode: {:?}", e);
            println!("\nThis is likely the cause of the 0x200d crash!");

            // Try to manually parse planes to find where decoding fails
            println!("\nManual plane parsing:");
            let plane_data = &data[1..]; // skip header byte
            println!("  Plane data: {} bytes", plane_data.len());

            // Try decode plane 1 (R) manually
            fn try_decode_plane(src: &[u8], width: usize, _height: usize) -> Result<usize, String> {
                // We can't call the private decompress_8bpp_plane directly.
                // Instead, try to parse the first scanline's control bytes manually.
                let mut i = 0usize;
                let mut col = 0usize;
                println!("    Parsing scanline 0:");
                while col < width && i < src.len() {
                    let ctrl = src[i];
                    i += 1;
                    if ctrl == 0 {
                        return Err(format!("Invalid control byte 0x00 at offset {}", i - 1));
                    }
                    let rle = (ctrl & 0x0F) as usize;
                    let raw = ((ctrl >> 4) & 0x0F) as usize;
                    let (run, raw_count) = match rle {
                        1 => (16 + raw, 0),
                        2 => (32 + raw, 0),
                        r => (r, raw),
                    };
                    println!(
                        "    ctrl=0x{ctrl:02x}: raw={raw_count} run={run} → {} bytes, col+={}",
                        raw_count + run,
                        raw_count + run
                    );
                    if i + raw_count > src.len() {
                        return Err(format!(
                            "Not enough bytes: need {} raw, have {}",
                            raw_count,
                            src.len() - i
                        ));
                    }
                    col += raw_count + run;
                    i += raw_count;
                    if col > width {
                        return Err(format!("Scanline overflow: col={} > width={}", col, width));
                    }
                }
                println!("    col after scanline 0: {} (expected {})", col, width);
                Ok(i)
            }

            let _ = try_decode_plane(plane_data, width, height);
        }
    }
}
