//! Synthesized demo media for the seed corpus.
//!
//! The seeded "generated image / speech / video" turns exist to exercise the
//! dashboard's media rendering, which only works if the bytes are REAL: a
//! 16-pixel PNG and a quarter-second beep technically decode but tell a
//! reader nothing, and a made-up URL never loads at all.
//!
//! Everything here is encoded from scratch with no image or audio crate:
//! - PNG via stored (uncompressed) DEFLATE blocks, which need no compressor.
//! - GIF via the standard "clear the table every run" LZW trick, which emits
//!   a valid stream without implementing dictionary compression.
//! - WAV as plain PCM.
//! - SVG as text, which is also the shape a model most often "draws" in.
//!
//! MP4 is deliberately absent: H.264 cannot be synthesized in a few hundred
//! lines, so the corpus keeps video-as-object-store-reference (the common
//! production shape) and uses an animated GIF where moving pictures need to
//! actually play.

/// Standard base64, the only encoder the crate needs.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    // (len + 2) / 3 rather than div_ceil: the crate's MSRV is 1.70.
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for group in bytes.chunks(3) {
        let b0 = u32::from(group[0]);
        let b1 = group.get(1).copied().map_or(0, u32::from);
        let b2 = group.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ------------------------------------------------------------------- chart

/// The bars the demo chart draws, as fractions of full height.
const BARS: [f32; 7] = [0.42, 0.65, 0.38, 0.81, 0.55, 0.93, 0.7];

/// Palette index for each role in the raster images.
const BG: u8 = 0;
const GRID: u8 = 1;
const BAR: u8 = 2;
const AXIS: u8 = 3;

fn palette() -> Vec<u8> {
    // Paper background, hairline grid, terracotta bars, ink axis.
    let colors: [(u8, u8, u8); 4] = [
        (0xFA, 0xF7, 0xF2),
        (0xE4, 0xDD, 0xD1),
        (0xC6, 0x5D, 0x3B),
        (0x1F, 0x1B, 0x17),
    ];
    let mut table = Vec::with_capacity(256 * 3);
    for index in 0..256 {
        let (r, g, b) = colors.get(index).copied().unwrap_or((0xFA, 0xF7, 0xF2));
        table.extend_from_slice(&[r, g, b]);
    }
    table
}

/// Renders the bar chart into an indexed pixel buffer. `grow` in 0.0..=1.0
/// scales the bars, which is what makes the animated frames move.
fn draw_chart(width: usize, height: usize, grow: f32) -> Vec<u8> {
    let mut pixels = vec![BG; width * height];
    let margin = height / 10;
    let baseline = height - margin;

    // Horizontal grid lines.
    for step in 1..5 {
        let y = margin + (baseline - margin) * step / 5;
        for x in margin..(width - margin) {
            pixels[y * width + x] = GRID;
        }
    }
    // Axes.
    for x in margin..(width - margin) {
        pixels[baseline * width + x] = AXIS;
    }
    for y in margin..=baseline {
        pixels[y * width + margin] = AXIS;
    }
    // Bars.
    let slot = (width - 2 * margin) / BARS.len();
    let bar_width = slot * 2 / 3;
    for (index, fraction) in BARS.iter().enumerate() {
        let scaled = (fraction * grow).clamp(0.0, 1.0);
        let bar_height = ((baseline - margin) as f32 * scaled) as usize;
        let left = margin + index * slot + (slot - bar_width) / 2;
        for y in (baseline - bar_height)..baseline {
            for x in left..(left + bar_width).min(width) {
                pixels[y * width + x] = BAR;
            }
        }
    }
    pixels
}

// --------------------------------------------------------------------- PNG

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(data.len() + 12);
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    chunk
}

/// A zlib stream of STORED deflate blocks — valid, and needs no compressor.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut offset = 0;
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    }
    while offset < raw.len() {
        let take = (raw.len() - offset).min(65535);
        let final_block = offset + take >= raw.len();
        out.push(if final_block { 1 } else { 0 });
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(&raw[offset..offset + take]);
        offset += take;
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// An 8-bit palette PNG of the demo chart.
pub(crate) fn png_chart(width: usize, height: usize) -> Vec<u8> {
    let pixels = draw_chart(width, height, 1.0);
    // Each scanline is prefixed with filter type 0 (None).
    let mut raw = Vec::with_capacity((width + 1) * height);
    for row in 0..height {
        raw.push(0);
        raw.extend_from_slice(&pixels[row * width..(row + 1) * width]);
    }

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 3, 0, 0, 0]); // 8-bit, colour type 3 (palette)
    png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    png.extend_from_slice(&png_chunk(b"PLTE", &palette()));
    png.extend_from_slice(&png_chunk(b"IDAT", &zlib_stored(&raw)));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    png
}

// --------------------------------------------------------------------- GIF

/// Packs LZW codes LSB-first.
struct BitWriter {
    out: Vec<u8>,
    bits: u32,
    count: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            bits: 0,
            count: 0,
        }
    }
    fn write(&mut self, code: u16, width: u32) {
        self.bits |= u32::from(code) << self.count;
        self.count += width;
        while self.count >= 8 {
            self.out.push((self.bits & 0xFF) as u8);
            self.bits >>= 8;
            self.count -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push((self.bits & 0xFF) as u8);
        }
        self.out
    }
}

/// Encodes one frame's pixels as an LZW stream that never grows past 9 bits:
/// the table is cleared before it would need a wider code. Valid, if larger
/// than a real compressor would produce.
fn gif_lzw(pixels: &[u8]) -> Vec<u8> {
    const CLEAR: u16 = 256;
    const END: u16 = 257;
    let mut writer = BitWriter::new();
    writer.write(CLEAR, 9);
    let mut emitted = 0;
    for pixel in pixels {
        writer.write(u16::from(*pixel), 9);
        emitted += 1;
        // After 253 codes the next entry would be 511, forcing 10-bit codes.
        if emitted == 250 {
            writer.write(CLEAR, 9);
            emitted = 0;
        }
    }
    writer.write(END, 9);
    writer.finish()
}

fn gif_sub_blocks(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 255 + 2);
    for chunk in data.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0);
    out
}

/// An animated GIF of the chart's bars growing — real moving pictures, which
/// is what a video slot needs in order to be worth rendering.
pub(crate) fn gif_animation(width: usize, height: usize, frames: usize) -> Vec<u8> {
    let mut gif = Vec::new();
    gif.extend_from_slice(b"GIF89a");
    gif.extend_from_slice(&(width as u16).to_le_bytes());
    gif.extend_from_slice(&(height as u16).to_le_bytes());
    gif.extend_from_slice(&[0xF7, 0x00, 0x00]); // global table, 256 entries
    gif.extend_from_slice(&palette());
    // Loop forever.
    gif.extend_from_slice(&[0x21, 0xFF, 0x0B]);
    gif.extend_from_slice(b"NETSCAPE2.0");
    gif.extend_from_slice(&[0x03, 0x01, 0x00, 0x00, 0x00]);

    for frame in 0..frames {
        let grow = (frame + 1) as f32 / frames as f32;
        let pixels = draw_chart(width, height, grow);
        // 12/100 s per frame.
        gif.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00, 12, 0x00, 0x00, 0x00]);
        gif.push(0x2C);
        gif.extend_from_slice(&0_u16.to_le_bytes());
        gif.extend_from_slice(&0_u16.to_le_bytes());
        gif.extend_from_slice(&(width as u16).to_le_bytes());
        gif.extend_from_slice(&(height as u16).to_le_bytes());
        gif.push(0x00);
        gif.push(0x08); // LZW minimum code size
        gif.extend_from_slice(&gif_sub_blocks(&gif_lzw(&pixels)));
    }
    gif.push(0x3B);
    gif
}

// --------------------------------------------------------------------- WAV

/// PCM WAV of a short arpeggio, long enough to have a real duration and be
/// worth pressing play on.
pub(crate) fn wav_arpeggio(seconds: f32) -> Vec<u8> {
    let sample_rate = 16_000_u32;
    let total = (sample_rate as f32 * seconds) as usize;
    let notes = [440.0_f32, 554.37, 659.25, 880.0];
    let per_note = total / notes.len();
    let mut samples: Vec<u8> = Vec::with_capacity(total);
    for index in 0..total {
        let note = (index / per_note.max(1)).min(notes.len() - 1);
        let t = index as f32 / sample_rate as f32;
        // Fade each note in and out so it does not click.
        let position = (index % per_note.max(1)) as f32 / per_note.max(1) as f32;
        let envelope = (position * std::f32::consts::PI).sin();
        let value = (2.0 * std::f32::consts::PI * notes[note] * t).sin() * envelope;
        samples.push((128.0 + 95.0 * value) as u8);
    }

    let mut wav = Vec::with_capacity(samples.len() + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1_u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&8_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    wav.extend_from_slice(&samples);
    wav
}

// --------------------------------------------------------------------- SVG

/// A crisp vector chart — tiny, and the form models most often produce when
/// asked to draw.
pub(crate) fn svg_chart() -> String {
    let (width, height) = (480.0_f32, 260.0_f32);
    let margin = 28.0_f32;
    let baseline = height - margin;
    let slot = (width - 2.0 * margin) / BARS.len() as f32;
    let mut bars = String::new();
    let labels = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul"];
    for (index, fraction) in BARS.iter().enumerate() {
        let bar_height = (baseline - margin) * fraction;
        let x = margin + index as f32 * slot + slot * 0.18;
        let w = slot * 0.64;
        let y = baseline - bar_height;
        bars.push_str(&format!(
            "<rect x='{x:.1}' y='{y:.1}' width='{w:.1}' height='{bar_height:.1}' rx='2' fill='#C65D3B'/>\
             <text x='{:.1}' y='{:.1}' font-family='monospace' font-size='10' fill='#6E675E' text-anchor='middle'>{}</text>",
            x + w / 2.0,
            baseline + 14.0,
            labels[index],
        ));
    }
    let mut grid = String::new();
    for step in 1..5 {
        let y = margin + (baseline - margin) * step as f32 / 5.0;
        grid.push_str(&format!(
            "<line x1='{margin:.1}' y1='{y:.1}' x2='{:.1}' y2='{y:.1}' stroke='#E4DDD1' stroke-width='1'/>",
            width - margin
        ));
    }
    format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{width:.0}' height='{height:.0}' viewBox='0 0 {width:.0} {height:.0}'>\
         <rect width='100%' height='100%' fill='#FAF7F2'/>{grid}\
         <text x='{margin:.1}' y='18' font-family='monospace' font-size='12' fill='#1F1B17'>Revenue by month</text>\
         {bars}\
         <line x1='{margin:.1}' y1='{baseline:.1}' x2='{:.1}' y2='{baseline:.1}' stroke='#1F1B17' stroke-width='1'/>\
         </svg>",
        width - margin
    )
}

/// `data:` URI for arbitrary bytes.
pub(crate) fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64_encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_a_valid_signature_and_chunks() {
        let png = png_chart(320, 180);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let text = String::from_utf8_lossy(&png);
        assert!(text.contains("IHDR") && text.contains("PLTE") && text.contains("IDAT"));
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // Big enough to be a real picture, small enough to inline.
        assert!(
            png.len() > 5_000 && png.len() < 200_000,
            "{} bytes",
            png.len()
        );
    }

    #[test]
    fn gif_is_animated_and_terminated() {
        let gif = gif_animation(128, 72, 6);
        assert_eq!(&gif[..6], b"GIF89a");
        assert_eq!(gif[gif.len() - 1], 0x3B, "GIF trailer");
        // One image descriptor per frame.
        assert!(gif.iter().filter(|byte| **byte == 0x2C).count() >= 6);
        assert!(gif.len() > 10_000, "an animation should carry real frames");
    }

    #[test]
    fn wav_declares_its_own_length() {
        let wav = wav_arpeggio(3.0);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let declared = u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]) as usize;
        assert_eq!(declared, wav.len() - 8, "RIFF size must match the file");
        // 3 s at 16 kHz mono 8-bit.
        assert!(wav.len() > 45_000, "{} bytes", wav.len());
    }

    #[test]
    fn svg_is_well_formed_enough_to_inline() {
        let svg = svg_chart();
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains("Revenue by month"));
        assert!(!svg.contains("<script"), "no scripting in demo art");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
