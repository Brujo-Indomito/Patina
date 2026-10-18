//! The app icon, drawn in code: a camera on a rounded blue square.

/// Returns `size` x `size` RGBA pixels.
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square background.
            let background = coverage(rounded_rect(px, py, 0.0, 0.0, s, s, s * 0.22));
            // Camera body and lens (lens drawn as a hole with a pupil).
            let body = coverage(rounded_rect(px, py, s * 0.16, s * 0.3, s * 0.68, s * 0.46, s * 0.08));
            let bump = coverage(rounded_rect(px, py, s * 0.36, s * 0.22, s * 0.28, s * 0.12, s * 0.04));
            let lens_distance = ((px - s * 0.5).powi(2) + (py - s * 0.53).powi(2)).sqrt();
            let lens_ring = coverage(lens_distance - s * 0.15);
            let pupil = coverage(lens_distance - s * 0.08);
            let white = (body.max(bump) * (1.0 - lens_ring)).max(pupil) * background;

            let blue = [37.0, 99.0, 235.0];
            let i = ((y * size + x) * 4) as usize;
            for c in 0..3 {
                pixels[i + c] = (blue[c] * (1.0 - white) + 255.0 * white) as u8;
            }
            pixels[i + 3] = (background * 255.0) as u8;
        }
    }
    pixels
}

/// Signed distance from (px, py) to a rounded rectangle (negative inside).
fn rounded_rect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (dx, dy) = ((px - cx).abs() - (w / 2.0 - r), (py - cy).abs() - (h / 2.0 - r));
    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    outside + dx.max(dy).min(0.0) - r
}

/// Anti-aliased coverage from a signed distance in pixels.
fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_has_opaque_center_and_transparent_corners() {
        let size = 32;
        let pixels = super::rgba(size);
        assert_eq!(pixels.len(), 32 * 32 * 4);
        assert_eq!(pixels[3], 0, "top-left corner should be transparent");
        let center = ((16 * size + 16) * 4) as usize;
        assert_eq!(pixels[center + 3], 255);
    }
}
