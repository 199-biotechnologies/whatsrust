//! QR code rendering in multiple formats for agent integration.
//!
//! Provides terminal (half-block Unicode), HTML, SVG, and PNG output
//! from raw QR data. Consumers subscribe to QR events via
//! `WhatsAppBridge::subscribe_qr()` and render in their preferred format.

use std::io::Write;
use std::path::Path;

use qrcode::QrCode;

/// Parsed QR code ready for multi-format rendering.
pub struct QrRender {
    data: String,
    modules: Vec<Vec<bool>>, // true = dark
    size: usize,
}

impl QrRender {
    /// Parse QR data into a renderable structure.
    pub fn new(data: &str) -> Option<Self> {
        let code = QrCode::new(data.as_bytes()).ok()?;
        let width = code.width();
        let colors = code.to_colors();
        let modules: Vec<Vec<bool>> = (0..width)
            .map(|y| {
                (0..width)
                    .map(|x| colors[y * width + x] == qrcode::Color::Dark)
                    .collect()
            })
            .collect();
        Some(Self {
            data: data.to_string(),
            modules,
            size: width,
        })
    }

    /// Raw QR data string (for consumers using their own QR library).
    pub fn data(&self) -> &str {
        &self.data
    }

    /// Module matrix (true = dark). For custom rendering.
    pub fn modules(&self) -> &[Vec<bool>] {
        &self.modules
    }

    /// QR module grid size (width == height).
    pub fn size(&self) -> usize {
        self.size
    }

    /// Compact terminal rendering using Unicode half-block characters.
    /// Packs two QR rows per terminal line, 1 char per module.
    /// With monospace ~2:1 height:width ratio and half-block vertical packing,
    /// each module renders as roughly square.
    pub fn terminal(&self) -> String {
        let quiet = 2; // compact quiet zone
        let total = self.size + 2 * quiet;
        let mut out = String::new();

        // Process rows in pairs
        let mut y = 0isize;
        while y < total as isize {
            out.push_str("\x1b[30;47m"); // black on white
            for x in 0..total as isize {
                let top = self.is_dark(x - quiet as isize, y - quiet as isize);
                let bot = self.is_dark(x - quiet as isize, y + 1 - quiet as isize);
                let ch = match (top, bot) {
                    (true, true) => '\u{2588}',   // █ full block
                    (true, false) => '\u{2580}',  // ▀ upper half
                    (false, true) => '\u{2584}',  // ▄ lower half
                    (false, false) => ' ',
                };
                out.push(ch);
            }
            out.push_str("\x1b[0m\n");
            y += 2;
        }

        out
    }

    /// Number of terminal lines the `terminal()` output occupies.
    /// Useful for ANSI cursor-up to overwrite a previous QR on refresh.
    pub fn terminal_lines(&self) -> usize {
        let quiet = 2;
        let total = self.size + 2 * quiet;
        (total + 1) / 2 // ceil(total / 2) since we pack 2 rows per line
    }

    /// Auto-refreshing HTML page with the QR code rendered as an inline SVG.
    /// The page reloads every 5 seconds to pick up QR refreshes from WhatsApp.
    /// Suitable for writing to a file and opening in a browser, or sharing with LLMs.
    pub fn html(&self) -> String {
        let svg = self.svg();
        format!(
            r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta http-equiv="refresh" content="5"><title>WhatsApp QR Code</title>
<style>body{{display:flex;justify-content:center;align-items:center;min-height:100vh;margin:0;background:#f0f0f0;font-family:system-ui}}
.container{{text-align:center;background:white;padding:32px;border-radius:16px;box-shadow:0 4px 24px rgba(0,0,0,.1)}}
h2{{color:#25D366;margin:0 0 16px}}svg{{max-width:320px;max-height:320px}}
p{{color:#666;margin:16px 0 0;font-size:14px}}.status{{color:#999;font-size:12px;margin-top:8px}}</style></head>
<body><div class="container"><h2>Scan with WhatsApp</h2>{svg}<p>Open WhatsApp &gt; Settings &gt; Linked Devices &gt; Link a Device</p><p class="status">Auto-refreshing every 5s</p></div></body></html>"#,
            svg = svg
        )
    }

    /// SVG string representing the QR code. Lightweight vector format, embeddable in HTML.
    pub fn svg(&self) -> String {
        let quiet = 4;
        let total = self.size + 2 * quiet;
        let mut paths = String::new();

        for y in 0..self.size {
            for x in 0..self.size {
                if self.modules[y][x] {
                    let px = x + quiet;
                    let py = y + quiet;
                    paths.push_str(&format!("M{px},{py}h1v1h-1z"));
                }
            }
        }

        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total} {total}" shape-rendering="crispEdges"><rect width="{total}" height="{total}" fill="white"/><path d="{paths}" fill="black"/></svg>"#,
            total = total,
            paths = paths
        )
    }

    /// PNG image bytes at the given scale (pixels per module).
    /// Scale of 8 gives good scannability (~300px for a typical QR).
    pub fn png(&self, scale: u32) -> Vec<u8> {
        let quiet = 4usize;
        let total = self.size + 2 * quiet;
        let img_size = total * scale as usize;

        // Build grayscale pixel data (white = 255, black = 0)
        let mut pixels = vec![255u8; img_size * img_size];
        for y in 0..self.size {
            for x in 0..self.size {
                if self.modules[y][x] {
                    let base_x = (x + quiet) * scale as usize;
                    let base_y = (y + quiet) * scale as usize;
                    for dy in 0..scale as usize {
                        for dx in 0..scale as usize {
                            pixels[(base_y + dy) * img_size + (base_x + dx)] = 0;
                        }
                    }
                }
            }
        }

        let mut buf = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut buf, img_size as u32, img_size as u32);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("PNG header");
            writer.write_image_data(&pixels).expect("PNG data");
        }
        buf
    }

    /// Save a PNG file at the given path. Scale = pixels per QR module.
    pub fn save_png(&self, path: &Path, scale: u32) -> std::io::Result<()> {
        let data = self.png(scale);
        let mut f = std::fs::File::create(path)?;
        f.write_all(&data)
    }

    /// Save an HTML file at the given path.
    pub fn save_html(&self, path: &Path) -> std::io::Result<()> {
        let html = self.html();
        let mut f = std::fs::File::create(path)?;
        f.write_all(html.as_bytes())
    }

    /// Helper: is the module at (x, y) dark? Out-of-bounds = light (quiet zone).
    fn is_dark(&self, x: isize, y: isize) -> bool {
        if x < 0 || y < 0 || x >= self.size as isize || y >= self.size as isize {
            return false;
        }
        self.modules[y as usize][x as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qr_render_basic() {
        let qr = QrRender::new("hello").expect("should parse");
        assert!(qr.size() > 0);
        assert_eq!(qr.modules().len(), qr.size());
        assert_eq!(qr.data(), "hello");
    }

    #[test]
    fn test_terminal_output() {
        let qr = QrRender::new("test").expect("should parse");
        let term = qr.terminal();
        assert!(!term.is_empty());
        // Should contain ANSI escape codes
        assert!(term.contains("\x1b["));
        // Should contain half-block characters
        assert!(term.contains('\u{2588}') || term.contains('\u{2580}') || term.contains('\u{2584}'));
    }

    #[test]
    fn test_svg_output() {
        let qr = QrRender::new("test").expect("should parse");
        let svg = qr.svg();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("viewBox"));
        assert!(svg.contains("fill=\"black\""));
    }

    #[test]
    fn test_html_output() {
        let qr = QrRender::new("test").expect("should parse");
        let html = qr.html();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<svg"));
    }

    #[test]
    fn test_png_output() {
        let qr = QrRender::new("test").expect("should parse");
        let png_data = qr.png(4);
        // PNG magic bytes
        assert_eq!(&png_data[..4], &[0x89, b'P', b'N', b'G']);
    }

}
