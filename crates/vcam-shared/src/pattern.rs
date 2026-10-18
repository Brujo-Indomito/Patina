//! Animated test pattern rendered into NV12 or RGB32 frames.
//!
//! [`Style::Live`] is what the app sends; [`Style::Idle`] is the dim fallback
//! the media source shows when no app is connected.

use crate::frame::{FrameFormat, PixelFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Live,
    Idle,
}

/// SMPTE-style bars at 75% intensity.
const LIVE_BARS: [[u8; 3]; 8] = [
    [191, 191, 191],
    [191, 191, 0],
    [0, 191, 191],
    [0, 191, 0],
    [191, 0, 191],
    [191, 0, 0],
    [0, 0, 191],
    [16, 16, 16],
];
const IDLE_BARS: [[u8; 3]; 8] = [[48, 48, 48]; 8];

const LIVE_BOX: [u8; 3] = [255, 120, 20];
const IDLE_BOX: [u8; 3] = [96, 96, 96];
const COUNTER_BITS: u32 = 24;

pub struct TestPattern {
    style: Style,
    frame: u64,
    row: Vec<[u8; 3]>,
}

impl TestPattern {
    pub fn new(style: Style) -> Self {
        Self { style, frame: 0, row: Vec::new() }
    }

    /// Renders the next frame into `dst`, which must hold `format.frame_bytes()` bytes.
    pub fn render(&mut self, format: &FrameFormat, dst: &mut [u8]) {
        let (w, h) = (format.width as usize, format.height as usize);
        self.row.resize(w, [0; 3]);

        let geometry = Geometry::new(format.width, format.height, self.frame, self.style);

        if format.pixel == PixelFormat::Rgb32 {
            for y in 0..h {
                geometry.fill_row(y as u32, self.frame, &mut self.row);
                let out = &mut dst[y * w * 4..(y + 1) * w * 4];
                for (px, rgb) in out.chunks_exact_mut(4).zip(&self.row) {
                    px.copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
                }
            }
        } else {
            let (y_plane, uv_plane) = dst.split_at_mut(w * h);
            for y in 0..h {
                geometry.fill_row(y as u32, self.frame, &mut self.row);
                let luma = &mut y_plane[y * w..(y + 1) * w];
                for (out, rgb) in luma.iter_mut().zip(&self.row) {
                    *out = rgb_to_y(*rgb);
                }
                if y % 2 == 0 {
                    let chroma = &mut uv_plane[(y / 2) * w..(y / 2) * w + w];
                    for (out, pair) in chroma.chunks_exact_mut(2).zip(self.row.chunks_exact(2)) {
                        let (u, v) = rgb_to_uv(average(pair[0], pair[1]));
                        out[0] = u;
                        out[1] = v;
                    }
                }
            }
        }

        self.frame += 1;
    }
}

/// Per-frame layout: bars on top, gray ramp with a binary frame counter below,
/// and a box bouncing across the whole frame.
struct Geometry {
    width: u32,
    bars_bottom: u32,
    counter_top: u32,
    counter_bottom: u32,
    cell_width: u32,
    box_x: u32,
    box_y: u32,
    box_size: u32,
    bars: &'static [[u8; 3]; 8],
    box_color: [u8; 3],
}

impl Geometry {
    fn new(width: u32, height: u32, frame: u64, style: Style) -> Self {
        let box_size = height / 6;
        Self {
            width,
            bars_bottom: height * 2 / 3,
            counter_top: height * 2 / 3 + height / 12,
            counter_bottom: height * 2 / 3 + height / 6,
            cell_width: width / COUNTER_BITS,
            box_x: bounce(frame * 7, width - box_size),
            box_y: bounce(frame * 4, height - box_size),
            box_size,
            bars: if style == Style::Live { &LIVE_BARS } else { &IDLE_BARS },
            box_color: if style == Style::Live { LIVE_BOX } else { IDLE_BOX },
        }
    }

    fn fill_row(&self, y: u32, frame: u64, row: &mut [[u8; 3]]) {
        let in_box_rows = y >= self.box_y && y < self.box_y + self.box_size;
        let in_counter_rows = y >= self.counter_top && y < self.counter_bottom;

        for (x, px) in row.iter_mut().enumerate() {
            let x = x as u32;
            *px = if in_box_rows && x >= self.box_x && x < self.box_x + self.box_size {
                self.box_color
            } else if y < self.bars_bottom {
                self.bars[(x * 8 / self.width) as usize]
            } else if in_counter_rows && x < self.cell_width * COUNTER_BITS {
                let bit = COUNTER_BITS - 1 - x / self.cell_width;
                let on = (frame >> bit) & 1 == 1;
                let edge = x % self.cell_width < 2;
                if edge {
                    [0, 0, 0]
                } else if on {
                    [235, 235, 235]
                } else {
                    [40, 40, 40]
                }
            } else {
                let level = (16 + x * 219 / self.width) as u8;
                [level, level, level]
            };
        }
    }
}

/// Triangle wave in `0..=range`.
fn bounce(position: u64, range: u32) -> u32 {
    if range == 0 {
        return 0;
    }
    let period = 2 * range as u64;
    let p = position % period;
    (if p > range as u64 { period - p } else { p }) as u32
}

fn average(a: [u8; 3], b: [u8; 3]) -> [u8; 3] {
    [
        ((a[0] as u16 + b[0] as u16) / 2) as u8,
        ((a[1] as u16 + b[1] as u16) / 2) as u8,
        ((a[2] as u16 + b[2] as u16) / 2) as u8,
    ]
}

/// BT.601 limited-range conversion.
fn rgb_to_y([r, g, b]: [u8; 3]) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

fn rgb_to_uv([r, g, b]: [u8; 3]) -> (u8, u8) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
    let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_both_formats_without_overrun() {
        for pixel in [PixelFormat::Nv12, PixelFormat::Rgb32] {
            for (width, height) in [(640, 360), (1280, 720), (1920, 1080)] {
                for style in [Style::Live, Style::Idle] {
                    let format = FrameFormat { pixel, width, height };
                    let mut buf = vec![0u8; format.frame_bytes()];
                    let mut pattern = TestPattern::new(style);
                    pattern.render(&format, &mut buf);
                    pattern.render(&format, &mut buf);
                    assert!(buf.iter().any(|&b| b != 0));
                }
            }
        }
    }

    #[test]
    fn white_and_black_map_to_limited_range() {
        assert_eq!(rgb_to_y([255, 255, 255]), 235);
        assert_eq!(rgb_to_y([0, 0, 0]), 16);
        assert_eq!(rgb_to_uv([128, 128, 128]), (128, 128));
    }
}
