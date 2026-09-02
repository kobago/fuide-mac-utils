//! Drag-out preview badge: a small FUI plate (dark fill, accent frame, seven-segment
//! item count) rendered into an RGBA buffer and encoded as an uncompressed PNG.
//! `NSDraggingSession` wants encoded image bytes; no image crate needed for a badge
//! this small. Rendered at 2x with a `pHYs` chunk (144 dpi) so it stays sharp on
//! Retina displays at its logical size.

/// Scale factor baked into the bitmap (compensated by the 144 dpi `pHYs` chunk).
const S: u32 = 2;

/// Encode the badge for `count` dragged items using the palette's accent color.
pub fn badge_png(count: usize, accent: [u8; 4]) -> Vec<u8> {
    let count = count.clamp(1, 999);
    let digits: Vec<u32> = {
        let mut v = Vec::new();
        let mut n = count;
        while n > 0 {
            v.push((n % 10) as u32);
            n /= 10;
        }
        v.reverse();
        v
    };

    // logical layout (pt): plate = pad | stack glyph | gap | digits | pad
    let (pad, glyph_w, gap, dig_w, dig_h, dig_gap, h) = (10, 16, 10, 13, 22, 5, 44);
    let w = pad + 4 + glyph_w + gap + digits.len() as u32 * (dig_w + dig_gap) - dig_gap + pad;

    let mut cv = Canvas::new(w * S, h * S);
    let bg = [8, 14, 20, 238];
    let dim = [accent[0], accent[1], accent[2], 90];

    // plate + glow (outer faint line) + frame + left bar
    cv.rect(0, 0, cv.w, cv.h, [accent[0], accent[1], accent[2], 36]);
    cv.rect(S, S, cv.w - S, cv.h - S, bg);
    cv.frame(S, S, cv.w - S, cv.h - S, S, accent);
    cv.rect(S, S, S + 3 * S, cv.h - S, accent);

    // stack glyph: three offset horizontal plates (a pile of files)
    let gx = (pad + 4) * S;
    let gy = (h / 2 - 9) * S;
    for i in 0..3u32 {
        let y = gy + i * 6 * S;
        let inset = (2 - i) * 2 * S;
        cv.frame(
            gx + inset,
            y,
            gx + glyph_w * S - inset,
            y + 4 * S,
            S,
            if i == 2 { accent } else { dim },
        );
    }

    // seven-segment count
    let mut x = (pad + 4 + glyph_w + gap) * S;
    let dy = (h - dig_h) / 2 * S;
    for d in digits {
        cv.seven_seg(x, dy, dig_w * S, dig_h * S, d, accent);
        x += (dig_w + dig_gap) * S;
    }

    encode_png(cv.w, cv.h, &cv.buf)
}

/// Bare RGBA raster with axis-aligned drawing (all this badge needs).
struct Canvas {
    w: u32,
    h: u32,
    buf: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            buf: vec![0u8; (w * h * 4) as usize],
        }
    }

    fn rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, c: [u8; 4]) {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = ((y * self.w + x) * 4) as usize;
                self.buf[i..i + 4].copy_from_slice(&c);
            }
        }
    }

    fn frame(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, t: u32, c: [u8; 4]) {
        self.rect(x0, y0, x1, y0 + t, c);
        self.rect(x0, y1 - t, x1, y1, c);
        self.rect(x0, y0, x0 + t, y1, c);
        self.rect(x1 - t, y0, x1, y1, c);
    }

    /// Segments: A top, B top-right, C bottom-right, D bottom, E bottom-left, F top-left, G middle.
    fn seven_seg(&mut self, x: u32, y: u32, w: u32, h: u32, d: u32, c: [u8; 4]) {
        let t = 2 * S; // segment thickness
        const SEGS: [[bool; 7]; 10] = [
            [true, true, true, true, true, true, false],     // 0
            [false, true, true, false, false, false, false], // 1
            [true, true, false, true, true, false, true],    // 2
            [true, true, true, true, false, false, true],    // 3
            [false, true, true, false, false, true, true],   // 4
            [true, false, true, true, false, true, true],    // 5
            [true, false, true, true, true, true, true],     // 6
            [true, true, true, false, false, false, false],  // 7
            [true, true, true, true, true, true, true],      // 8
            [true, true, true, true, false, true, true],     // 9
        ];
        let s = SEGS[d as usize];
        let mid = y + (h - t) / 2;
        if s[0] {
            self.rect(x + t, y, x + w - t, y + t, c);
        }
        if s[1] {
            self.rect(x + w - t, y + t, x + w, mid, c);
        }
        if s[2] {
            self.rect(x + w - t, mid + t, x + w, y + h - t, c);
        }
        if s[3] {
            self.rect(x + t, y + h - t, x + w - t, y + h, c);
        }
        if s[4] {
            self.rect(x, mid + t, x + t, y + h - t, c);
        }
        if s[5] {
            self.rect(x, y + t, x + t, mid, c);
        }
        if s[6] {
            self.rect(x + t, mid, x + w - t, mid + t, c);
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal PNG writer (RGBA8, zlib "stored" blocks — no compression, no deps)

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut out, b"IHDR", &ihdr);

    // 144 dpi = 5669 px/m: tells AppKit the image is 2x (logical size = pixels / 2)
    let ppm = (5669u32).to_be_bytes();
    let mut phys = Vec::with_capacity(9);
    phys.extend_from_slice(&ppm);
    phys.extend_from_slice(&ppm);
    phys.push(1);
    chunk(&mut out, b"pHYs", &phys);

    // scanlines with filter byte 0
    let stride = (w * 4) as usize;
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for row in rgba.chunks_exact(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut z = Vec::with_capacity(data.len() + data.len() / 65535 * 5 + 16);
    z.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = data.chunks(65535).peekable();
    loop {
        let Some(c) = chunks.next() else {
            // empty input still needs one final stored block
            z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
            break;
        };
        let last = chunks.peek().is_none();
        z.push(last as u8);
        let len = c.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(c);
        if last {
            break;
        }
    }
    // adler32 over the uncompressed data
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    z
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }
    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 ^= byte as u32;
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }
    fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_is_a_wellformed_png_with_expected_dimensions() {
        for count in [1, 7, 42, 999, 5000] {
            let png = badge_png(count, [0, 229, 255, 255]);
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
            assert_eq!(&png[12..16], b"IHDR");
            let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
            assert_eq!(h, 44 * S);
            let ndigits = count.clamp(1, 999).to_string().len() as u32;
            assert_eq!(w, (10 + 4 + 16 + 10 + ndigits * 18 - 5 + 10) * S);
            assert!(png.ends_with(&[0xAE, 0x42, 0x60, 0x82]), "IEND crc");
        }
    }

    /// Dev aid: `cargo test -p fuide-file-manager dump_badge -- --ignored` writes sample
    /// badges to `/tmp/fuide-badge-*.png` for a visual check.
    #[test]
    #[ignore]
    fn dump_badge_for_inspection() {
        for (count, name) in [(1, "1"), (12, "12"), (345, "345")] {
            let path = format!("/tmp/fuide-badge-{name}.png");
            std::fs::write(&path, badge_png(count, [0, 229, 255, 255])).unwrap();
        }
    }

    #[test]
    fn zlib_stream_carries_the_raw_bytes() {
        let data = vec![7u8; 100];
        let z = zlib_stored(&data);
        assert_eq!(&z[..2], &[0x78, 0x01]);
        assert_eq!(z[2], 1, "single final block");
        assert_eq!(u16::from_le_bytes([z[3], z[4]]), 100);
        assert_eq!(&z[7..107], &data[..]);
    }
}
