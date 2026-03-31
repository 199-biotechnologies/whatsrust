//! Media processing utilities for outbound message enrichment.
//!
//! - Image: dimension extraction + JPEG thumbnail generation
//! - Audio: waveform visualization data (64 amplitude samples)

/// Image dimensions and optional JPEG thumbnail.
pub struct ImageMeta {
    pub width: u32,
    pub height: u32,
    pub thumbnail: Vec<u8>,
}

/// Generate image metadata: dimensions + small JPEG thumbnail (~32px).
/// Returns None if the image can't be decoded (corrupt or unsupported format).
pub fn extract_image_meta(data: &[u8]) -> Option<ImageMeta> {
    let img = image::load_from_memory(data).ok()?;
    let (w, h) = (img.width(), img.height());

    // Generate thumbnail: 32px on longest side, JPEG quality 50
    let thumb = img.thumbnail(32, 32);
    let mut buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 50);
    thumb.write_with_encoder(encoder).ok()?;

    Some(ImageMeta {
        width: w,
        height: h,
        thumbnail: buf.into_inner(),
    })
}

/// Extract just image dimensions from raw bytes (fast path using image crate).
pub fn extract_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // Try header-only decode first for speed
    #[allow(deprecated)]
    let reader = image::io::Reader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    if let Ok((w, h)) = reader.into_dimensions() {
        return Some((w, h));
    }
    // Fallback: full decode
    let img = image::load_from_memory(data).ok()?;
    Some((img.width(), img.height()))
}

/// Generate audio waveform visualization data.
///
/// Returns exactly 64 bytes, each 0-100, representing amplitude at evenly-spaced
/// intervals across the audio. This matches the WhatsApp waveform format.
///
/// Works with raw OGG/Opus data by treating it as a statistical byte distribution
/// (not a proper audio decode — but produces visually reasonable waveforms without
/// adding an audio codec dependency).
///
/// For proper PCM-based waveforms, an audio decoder (symphonia, etc.) would be needed,
/// but the binary size cost (~1-2MB) isn't justified for a cosmetic feature.
pub fn generate_waveform(audio_data: &[u8]) -> Vec<u8> {
    const SAMPLES: usize = 64;

    if audio_data.is_empty() {
        return vec![0u8; SAMPLES];
    }

    // Skip the OGG header (~200 bytes) to get to audio frame data
    let skip = audio_data.len().min(200);
    let payload = &audio_data[skip..];

    if payload.is_empty() {
        return vec![0u8; SAMPLES];
    }

    let block_size = payload.len() / SAMPLES;
    if block_size == 0 {
        return vec![50u8; SAMPLES]; // Very short audio — flat middle line
    }

    // Calculate RMS-like amplitude per block
    let mut amplitudes: Vec<f32> = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        let start = i * block_size;
        let end = (start + block_size).min(payload.len());
        let block = &payload[start..end];

        // Treat bytes as unsigned samples centered at 128
        let rms: f32 = block.iter()
            .map(|&b| {
                let centered = (b as f32) - 128.0;
                centered * centered
            })
            .sum::<f32>() / block.len() as f32;
        amplitudes.push(rms.sqrt());
    }

    // Normalize to 0-100 range
    let max_amp = amplitudes.iter().cloned().fold(0.0f32, f32::max);
    if max_amp < 1.0 {
        return vec![5u8; SAMPLES]; // Silence — minimal flat line
    }

    amplitudes.iter()
        .map(|&a| ((a / max_amp) * 100.0).clamp(0.0, 100.0) as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_waveform_length() {
        let data = vec![128u8; 10000];
        let wf = generate_waveform(&data);
        assert_eq!(wf.len(), 64);
    }

    #[test]
    fn test_waveform_empty() {
        let wf = generate_waveform(&[]);
        assert_eq!(wf.len(), 64);
        assert!(wf.iter().all(|&v| v == 0));
    }

    #[test]
    fn test_waveform_values_in_range() {
        // Simulate varying audio data
        let data: Vec<u8> = (0..20000).map(|i| ((i * 7 + 13) % 256) as u8).collect();
        let wf = generate_waveform(&data);
        assert_eq!(wf.len(), 64);
        assert!(wf.iter().all(|&v| v <= 100));
        // Should have some variation
        let min = *wf.iter().min().unwrap();
        let max = *wf.iter().max().unwrap();
        assert!(max > min, "waveform should have variation: min={min}, max={max}");
    }

    #[test]
    fn test_image_dimensions_png() {
        // Minimal 1x1 white PNG
        let png_data = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // IHDR
            0x00, 0x00, 0x00, 0x01, // width=1
            0x00, 0x00, 0x00, 0x01, // height=1
            0x08, 0x02, // bit depth=8, color type=RGB
            0x00, 0x00, 0x00, // compression, filter, interlace
            0x90, 0x77, 0x53, 0xDE, // CRC
            0x00, 0x00, 0x00, 0x0C, // IDAT length
            0x49, 0x44, 0x41, 0x54, // IDAT
            0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00,
            0x00, 0x02, 0x00, 0x01, // compressed data
            0xE2, 0x21, 0xBC, 0x33, // CRC
            0x00, 0x00, 0x00, 0x00, // IEND length
            0x49, 0x45, 0x4E, 0x44, // IEND
            0xAE, 0x42, 0x60, 0x82, // CRC
        ];
        let dims = extract_image_dimensions(&png_data);
        assert_eq!(dims, Some((1, 1)));
    }
}
