//! Background effects: blur or replace everything the segmentation matte marks
//! as background. Works directly on NV12 frames on the CPU.
//!
//! Per frame:
//! 1. The frame goes to the segmenter. With a GPU backend the matte for this
//!    very frame is ready within a few milliseconds, so we wait for it (no
//!    lag, no trails); on the CPU we use the newest matte available.
//! 2. The matte is smoothed over time only where it barely changes (removes
//!    flicker without dragging a trail behind movement), then sharpened with a
//!    smoothstep so uncertain areas inside the person become fully opaque while
//!    the edge stays soft.
//! 3. It is upscaled to luma and chroma resolution and used to blend the frame
//!    over a background. Blur builds that background with a *masked* blur at
//!    quarter resolution: the person is weighted out before blurring and the
//!    result renormalized, so the person's colors never smear into the
//!    background (no aura).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use vcam_shared::{FrameFormat, PixelFormat};

use crate::segment::{Backend, Matte, ModelKind, Segmenter};

/// Background blur works at 1/BLUR_SCALE resolution.
const BLUR_SCALE: usize = 4;
/// How long to wait for a frame's matte on a GPU backend. With pipelining the
/// model has had a whole camera frame interval already, so this rarely waits.
const MATTE_WAIT: Duration = Duration::from_millis(30);
/// A held frame older than this (the camera stopped in between) is dropped.
const PIPELINE_MAX_AGE: Duration = Duration::from_millis(250);
/// Weight of the newest matte where it changes little (reduces flicker).
const MATTE_SMOOTHING: f32 = 0.5;
/// Changes larger than this are movement and are taken immediately.
const MOTION_THRESHOLD: f32 = 0.2;

/// How a model's raw matte becomes the blend alpha.
struct MatteTuning {
    /// Apply `smooth_matte` (single-frame models flicker; RVM is recurrent).
    smooth: bool,
    /// Smoothstep range: values below `low` become background, above `high` person.
    low: f32,
    high: f32,
}

impl MatteTuning {
    fn for_model(kind: ModelKind) -> Self {
        match kind {
            // MODNet is uncertain inside the person in low light: push it to opaque.
            ModelKind::Modnet => Self { smooth: true, low: 0.2, high: 0.6 },
            // RVM's alpha is a calibrated matte: only trim near-0/near-1 noise
            // so hair and edges keep their softness.
            ModelKind::Rvm => Self { smooth: false, low: 0.05, high: 0.95 },
        }
    }
}
/// Below this background weight a blurred sample is mostly person; fall back
/// to the plain blur there.
const MIN_BACKGROUND_WEIGHT: f32 = 0.05;

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Blur radius in quarter-resolution pixels, 1..=20.
    Blur { strength: u32 },
    /// Replace the background with an image (JPEG or PNG), scaled to cover.
    Replace { image: PathBuf },
    /// Debug view: output the blend alpha as grayscale (white = person).
    ShowMask,
}

impl std::fmt::Display for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::Blur { strength } => write!(f, "background blur (strength {strength})"),
            Effect::Replace { image } => write!(f, "background image {}", image.display()),
            Effect::ShowMask => write!(f, "mask view (white = person)"),
        }
    }
}

pub struct Effects {
    effect: Effect,
    segmenter: Segmenter,
    /// Decoded replacement image, RGB, and its NV12 version for the current size.
    image: Option<image::RgbImage>,
    image_planes: Option<(usize, usize, Planes)>,
    matte_source: Option<Arc<Matte>>,
    smoothed: Vec<f32>,
    /// `smoothed` after the smoothstep: the blend alpha at model resolution.
    alpha: Vec<f32>,
    alpha_luma: Vec<u8>,
    alpha_chroma: Vec<u8>,
    frame_planes: Planes,
    background: Planes,
    /// One set per plane (Y, U, V) so the three blurs can run in parallel.
    blur: [BlurBuffers; 3],
    warned_format: bool,
    reported_backend: bool,
    /// Lowest and highest person coverage since the last report.
    coverage_range: Option<(f32, f32)>,
    /// Frames between each composited frame and the frame its mask came
    /// from, summed and maxed since the last report.
    lag_sum: u64,
    lag_max: u64,
    lag_frames: u64,
    /// The frame submitted last, composited on the next call (see `process`).
    pending: Option<Pending>,
    timings: StageTimings,
}

/// A frame waiting one call for its matte.
struct Pending {
    number: u64,
    format: FrameFormat,
    frame: Vec<u8>,
    submitted: Instant,
}

/// Time spent per stage since the last report.
#[derive(Default)]
struct StageTimings {
    submit: Duration,
    wait: Duration,
    composite: Duration,
    frames: u32,
}

/// Separate Y, U and V planes (U and V at half resolution).
#[derive(Default)]
struct Planes {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

/// Reused quarter-resolution buffers for the masked blur.
#[derive(Default)]
struct BlurBuffers {
    /// Background-weighted sum, background weight, and unweighted value.
    weighted: Vec<f32>,
    weight: Vec<f32>,
    plain: Vec<f32>,
    result: Vec<f32>,
    scratch: Vec<f32>,
}

impl Effects {
    pub fn new(effect: Effect, model: PathBuf, allow_gpu: bool) -> Result<Self, String> {
        let image = match &effect {
            Effect::Replace { image } => Some(load_image(image)?),
            Effect::Blur { .. } | Effect::ShowMask => None,
        };
        Ok(Self {
            effect,
            segmenter: Segmenter::start(model, allow_gpu),
            image,
            image_planes: None,
            matte_source: None,
            smoothed: Vec::new(),
            alpha: Vec::new(),
            alpha_luma: Vec::new(),
            alpha_chroma: Vec::new(),
            frame_planes: Planes::default(),
            background: Planes::default(),
            blur: Default::default(),
            warned_format: false,
            reported_backend: false,
            coverage_range: None,
            lag_sum: 0,
            lag_max: 0,
            lag_frames: 0,
            pending: None,
            timings: StageTimings::default(),
        })
    }

    /// Mean milliseconds per frame for (preparing and submitting the model
    /// input, waiting for the matte, compositing) since the last call.
    pub fn take_timings(&mut self) -> Option<(f64, f64, f64)> {
        let t = std::mem::take(&mut self.timings);
        let per_frame = |d: Duration| d.as_secs_f64() * 1000.0 / t.frames as f64;
        (t.frames > 0).then(|| (per_frame(t.submit), per_frame(t.wait), per_frame(t.composite)))
    }

    pub fn effect(&self) -> &Effect {
        &self.effect
    }

    /// Switches the effect without reloading the model.
    pub fn set_effect(&mut self, effect: Effect) -> Result<(), String> {
        self.image = match &effect {
            Effect::Replace { image } => Some(load_image(image)?),
            Effect::Blur { .. } | Effect::ShowMask => None,
        };
        self.image_planes = None;
        self.effect = effect;
        Ok(())
    }

    /// "RVM fp16 on DirectML (GPU)" once the model has loaded.
    pub fn description(&self) -> Option<String> {
        Some(format!("{} on {}", self.segmenter.label()?, self.segmenter.backend()?))
    }

    pub fn backend(&self) -> Option<Backend> {
        self.segmenter.backend()
    }

    /// (inferences, mean ms) since the last call.
    pub fn take_stats(&self) -> (u64, f64) {
        self.segmenter.take_stats()
    }

    /// (mean, max) frames the mask lagged behind the video since the last call.
    pub fn take_lag(&mut self) -> Option<(f64, u64)> {
        let result = (self.lag_frames > 0).then(|| (self.lag_sum as f64 / self.lag_frames as f64, self.lag_max));
        (self.lag_sum, self.lag_max, self.lag_frames) = (0, 0, 0);
        result
    }

    /// Lowest and highest fraction of the frame marked as person since the
    /// last call; a dip to near zero means the mask collapsed.
    pub fn take_coverage_range(&mut self) -> Option<(f32, f32)> {
        self.coverage_range.take()
    }

    /// Applies the effect to a one-frame pipeline: `frame` (frame N) is
    /// submitted for segmentation and kept, and on return `frame` holds frame
    /// N-1 composited with its own matte, which the model computed while the
    /// camera was delivering frame N. That keeps masks exactly matched to
    /// their frames at full frame rate, for one frame of extra latency.
    ///
    /// Frames pass through unchanged until the first matte is ready, and if
    /// the format isn't NV12.
    pub fn process(&mut self, format: FrameFormat, frame: &mut Vec<u8>) {
        if format.pixel != PixelFormat::Nv12 {
            if !self.warned_format {
                println!("Background effects support NV12 output only; sending {format} unprocessed.");
                self.warned_format = true;
            }
            self.pending = None;
            return;
        }
        let (w, h) = (format.width as usize, format.height as usize);

        let started = Instant::now();
        let number = self.segmenter.submit(frame, w, h);
        self.timings.submit += started.elapsed();
        if let Some(err) = self.segmenter.take_error() {
            println!("{err}");
        }
        let backend = self.segmenter.backend();
        if !self.reported_backend {
            if let Some(backend) = backend {
                let model = self.segmenter.label().unwrap_or_else(|| "model".to_string());
                println!("Segmentation: {model} on {backend}.");
                self.reported_backend = true;
            }
        }

        // Keep frame N; continue with frame N-1 in `frame`. A missing, stale
        // (camera restarted) or differently sized previous frame restarts the
        // pipeline by sending frame N as is.
        let current = Pending { number, format, frame: std::mem::take(frame), submitted: Instant::now() };
        let previous = self.pending.replace(current);
        let previous = match previous {
            Some(previous) if previous.format == format && previous.submitted.elapsed() < PIPELINE_MAX_AGE => previous,
            _ => {
                frame.extend_from_slice(&self.pending.as_ref().expect("just stored").frame);
                return;
            }
        };
        *frame = previous.frame;
        let number = previous.number;

        let waiting = Instant::now();
        let matte = match backend {
            Some(Backend::DirectMl) => self.segmenter.wait_for(number, MATTE_WAIT),
            _ => self.segmenter.matte(),
        };
        self.timings.wait += waiting.elapsed();
        let Some(matte) = matte else { return };
        let compositing = Instant::now();
        self.composite(w, h, number, matte, frame);
        self.timings.composite += compositing.elapsed();
        self.timings.frames += 1;
    }

    fn composite(&mut self, w: usize, h: usize, number: u64, matte: Arc<Matte>, frame: &mut [u8]) {
        let (mw, mh) = (matte.width, matte.height);
        let lag = number.saturating_sub(matte.frame);
        self.lag_sum += lag;
        self.lag_max = self.lag_max.max(lag);
        self.lag_frames += 1;
        self.update_alpha(matte);

        resize_f32_to_u8(&self.alpha, mw, mh, &mut self.alpha_luma, w, h);
        resize_f32_to_u8(&self.alpha, mw, mh, &mut self.alpha_chroma, w / 2, h / 2);

        split_nv12(frame, w, h, &mut self.frame_planes);
        match &self.effect {
            Effect::Blur { strength } => {
                let radius = (*strength).clamp(1, 20) as usize;
                self.build_blurred_background(w, h, radius);
                blend_nv12(frame, w, h, &self.alpha_luma, &self.alpha_chroma, &self.background);
            }
            Effect::Replace { .. } => {
                let stale = !matches!(&self.image_planes, Some((iw, ih, _)) if (*iw, *ih) == (w, h));
                if stale {
                    if let Some(image) = &self.image {
                        self.image_planes = Some((w, h, image_to_planes(image, w, h)));
                    }
                }
                if let Some((_, _, planes)) = &self.image_planes {
                    blend_nv12(frame, w, h, &self.alpha_luma, &self.alpha_chroma, planes);
                }
            }
            Effect::ShowMask => {
                let (luma, chroma) = frame.split_at_mut(w * h);
                for (out, &a) in luma.iter_mut().zip(&self.alpha_luma) {
                    *out = 16 + ((a as u32 * 219 + 127) / 255) as u8;
                }
                chroma.fill(128);
            }
        }
    }

    fn update_alpha(&mut self, matte: Arc<Matte>) {
        if self.matte_source.as_ref().is_some_and(|previous| Arc::ptr_eq(previous, &matte)) {
            return;
        }
        let tuning = MatteTuning::for_model(self.segmenter.kind().unwrap_or(ModelKind::Modnet));
        let source = if tuning.smooth {
            smooth_matte(&mut self.smoothed, &matte.alpha);
            &self.smoothed
        } else {
            &matte.alpha
        };
        self.alpha.clear();
        self.alpha.extend(source.iter().map(|&a| smoothstep(tuning.low, tuning.high, a)));
        self.matte_source = Some(matte);

        let coverage = self.alpha.iter().sum::<f32>() / self.alpha.len().max(1) as f32;
        self.coverage_range = Some(match self.coverage_range {
            Some((low, high)) => (low.min(coverage), high.max(coverage)),
            None => (coverage, coverage),
        });
    }

    fn build_blurred_background(&mut self, w: usize, h: usize, radius: usize) {
        let (cw, ch) = (w / 2, h / 2);
        let chroma_radius = (radius / 2).max(1);
        let planes = &self.frame_planes;
        let (alpha_luma, alpha_chroma) = (&self.alpha_luma, &self.alpha_chroma);
        let [blur_y, blur_u, blur_v] = &mut self.blur;
        let Planes { y: bg_y, u: bg_u, v: bg_v } = &mut self.background;
        rayon::join(
            || masked_blur(&planes.y, alpha_luma, w, h, radius, blur_y, bg_y),
            || {
                rayon::join(
                    || masked_blur(&planes.u, alpha_chroma, cw, ch, chroma_radius, blur_u, bg_u),
                    || masked_blur(&planes.v, alpha_chroma, cw, ch, chroma_radius, blur_v, bg_v),
                )
            },
        );
    }
}

/// Temporal smoothing that follows movement: large changes are taken at once,
/// small ones (noise, flicker) are averaged.
fn smooth_matte(smoothed: &mut Vec<f32>, matte: &[f32]) {
    if smoothed.len() != matte.len() {
        smoothed.clear();
        smoothed.extend_from_slice(matte);
        return;
    }
    for (s, &m) in smoothed.iter_mut().zip(matte) {
        let change = m - *s;
        *s += if change.abs() > MOTION_THRESHOLD { change } else { MATTE_SMOOTHING * change };
    }
}

fn smoothstep(low: f32, high: f32, x: f32) -> f32 {
    let t = ((x - low) / (high - low)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Blurs `src` (`w` x `h`) at 1/BLUR_SCALE resolution while excluding the
/// person given by `alpha`, and writes the full-resolution result to `dst`.
fn masked_blur(src: &[u8], alpha: &[u8], w: usize, h: usize, radius: usize, b: &mut BlurBuffers, dst: &mut Vec<u8>) {
    let (sw, sh) = (w / BLUR_SCALE, h / BLUR_SCALE);
    if sw == 0 || sh == 0 {
        dst.clear();
        dst.extend_from_slice(src);
        return;
    }
    downscale_weighted(src, alpha, w, h, b);
    for _ in 0..3 {
        box_blur(&mut b.weighted, sw, sh, radius, &mut b.scratch);
        box_blur(&mut b.weight, sw, sh, radius, &mut b.scratch);
        box_blur(&mut b.plain, sw, sh, radius, &mut b.scratch);
    }
    b.result.resize(sw * sh, 0.0);
    for i in 0..sw * sh {
        let weight = b.weight[i];
        let background_only = b.weighted[i] / weight.max(1e-6);
        // Blend toward the plain blur where almost no background contributed.
        let t = (weight / MIN_BACKGROUND_WEIGHT).min(1.0);
        b.result[i] = t * background_only + (1.0 - t) * b.plain[i];
    }
    resize_f32_to_u8(&b.result, sw, sh, dst, w, h);
}

/// Averages BLUR_SCALE x BLUR_SCALE blocks into [0, 1] values: the
/// background-weighted value, the background weight `(1 - alpha)^2` (squared
/// to also keep soft edge pixels out of the background), and the plain value.
fn downscale_weighted(src: &[u8], alpha: &[u8], w: usize, h: usize, b: &mut BlurBuffers) {
    let (sw, sh) = (w / BLUR_SCALE, h / BLUR_SCALE);
    for buffer in [&mut b.weighted, &mut b.weight, &mut b.plain] {
        buffer.clear();
        buffer.resize(sw * sh, 0.0);
    }
    let scale = 1.0 / (BLUR_SCALE * BLUR_SCALE) as f32 / 255.0;
    // One output row per task; each reads its BLUR_SCALE source rows.
    b.weighted
        .par_chunks_mut(sw)
        .zip(b.weight.par_chunks_mut(sw))
        .zip(b.plain.par_chunks_mut(sw))
        .enumerate()
        .for_each(|(oy, ((weighted, weight_row), plain))| {
            for y in oy * BLUR_SCALE..(oy + 1) * BLUR_SCALE {
                let row = &src[y * w..y * w + sw * BLUR_SCALE];
                let alpha_row = &alpha[y * w..y * w + sw * BLUR_SCALE];
                for (x, (&value, &a)) in row.iter().zip(alpha_row).enumerate() {
                    let i = x / BLUR_SCALE;
                    let background = (255 - a) as f32 / 255.0;
                    let weight = background * background;
                    weighted[i] += value as f32 * weight * scale;
                    weight_row[i] += weight * scale * 255.0;
                    plain[i] += value as f32 * scale;
                }
            }
        });
}

pub fn load_image(path: &Path) -> Result<image::RgbImage, String> {
    image::open(path)
        .map(|img| img.to_rgb8())
        .map_err(|e| format!("cannot load background image {}: {e}", path.display()))
}

/// Scales `image` to cover `w` x `h` (cropping the overflow) and converts it to planes.
fn image_to_planes(image: &image::RgbImage, w: usize, h: usize) -> Planes {
    let (iw, ih) = (image.width() as f64, image.height() as f64);
    let scale = (w as f64 / iw).max(h as f64 / ih);
    let (crop_w, crop_h) = (w as f64 / scale, h as f64 / scale);
    let (x0, y0) = ((iw - crop_w) / 2.0, (ih - crop_h) / 2.0);

    let mut rgb = vec![[0u8; 3]; w * h];
    for y in 0..h {
        let sy = ((y0 + (y as f64 + 0.5) / scale) as u32).min(image.height() - 1);
        for x in 0..w {
            let sx = ((x0 + (x as f64 + 0.5) / scale) as u32).min(image.width() - 1);
            rgb[y * w + x] = image.get_pixel(sx, sy).0;
        }
    }

    let (cw, ch) = (w / 2, h / 2);
    let mut planes = Planes { y: vec![0; w * h], u: vec![0; cw * ch], v: vec![0; cw * ch] };
    for (out, px) in planes.y.iter_mut().zip(&rgb) {
        let [r, g, b] = px.map(i32::from);
        *out = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    }
    for cy in 0..ch {
        for cx in 0..cw {
            let mut sum = [0i32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let px = rgb[(cy * 2 + dy) * w + cx * 2 + dx];
                for c in 0..3 {
                    sum[c] += px[c] as i32;
                }
            }
            let [r, g, b] = sum.map(|s| s / 4);
            planes.u[cy * cw + cx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            planes.v[cy * cw + cx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
    planes
}

fn split_nv12(frame: &[u8], w: usize, h: usize, planes: &mut Planes) {
    let (cw, ch) = (w / 2, h / 2);
    planes.y.clear();
    planes.y.extend_from_slice(&frame[..w * h]);
    planes.u.resize(cw * ch, 0);
    planes.v.resize(cw * ch, 0);
    let uv_rows = frame[w * h..w * h + cw * ch * 2].par_chunks(cw * 2);
    planes.u.par_chunks_mut(cw).zip(planes.v.par_chunks_mut(cw)).zip(uv_rows).for_each(|((u, v), uv)| {
        for ((u, v), pair) in u.iter_mut().zip(v.iter_mut()).zip(uv.chunks_exact(2)) {
            *u = pair[0];
            *v = pair[1];
        }
    });
}

/// `frame = alpha * frame + (1 - alpha) * background`, per plane, row-parallel.
fn blend_nv12(frame: &mut [u8], w: usize, h: usize, alpha_luma: &[u8], alpha_chroma: &[u8], background: &Planes) {
    let cw = w / 2;
    let (luma, chroma) = frame.split_at_mut(w * h);
    luma.par_chunks_mut(w).zip(alpha_luma.par_chunks(w)).zip(background.y.par_chunks(w)).for_each(|((fg, a), bg)| {
        for ((fg, &a), &bg) in fg.iter_mut().zip(a).zip(bg) {
            *fg = mix(*fg, bg, a);
        }
    });
    chroma[..cw * (h / 2) * 2]
        .par_chunks_mut(cw * 2)
        .zip(alpha_chroma.par_chunks(cw))
        .zip(background.u.par_chunks(cw))
        .zip(background.v.par_chunks(cw))
        .for_each(|(((uv, a), bu), bv)| {
            for (i, pair) in uv.chunks_exact_mut(2).enumerate() {
                pair[0] = mix(pair[0], bu[i], a[i]);
                pair[1] = mix(pair[1], bv[i], a[i]);
            }
        });
}

#[inline]
fn mix(fg: u8, bg: u8, alpha: u8) -> u8 {
    let a = alpha as u32;
    let x = fg as u32 * a + bg as u32 * (255 - a) + 128;
    ((x + (x >> 8)) >> 8) as u8
}

/// One horizontal and one vertical box-blur pass with edge clamping.
fn box_blur(plane: &mut [f32], w: usize, h: usize, radius: usize, scratch: &mut Vec<f32>) {
    if w == 0 || h == 0 || radius == 0 {
        return;
    }
    scratch.resize(plane.len(), 0.0);
    blur_lines(plane, scratch, w, h, 1, w, radius);
    blur_lines(scratch, plane, h, w, w, 1, radius);
}

/// Blurs `lines` lines of `len` samples (`step` apart, lines `line_step` apart) from `src` into `dst`.
fn blur_lines(src: &[f32], dst: &mut [f32], len: usize, lines: usize, step: usize, line_step: usize, radius: usize) {
    let window = (2 * radius + 1) as f32;
    let at = |line: usize, i: isize| -> f32 {
        let i = i.clamp(0, len as isize - 1) as usize;
        src[line * line_step + i * step]
    };
    for line in 0..lines {
        let mut sum: f32 = (-(radius as isize)..=radius as isize).map(|i| at(line, i)).sum();
        for i in 0..len {
            dst[line * line_step + i * step] = sum / window;
            sum += at(line, i as isize + radius as isize + 1);
            sum -= at(line, i as isize - radius as isize);
        }
    }
}

/// Precomputed bilinear sampling positions along one axis.
fn axis(src_len: usize, dst_len: usize) -> Vec<(usize, usize, u32)> {
    (0..dst_len)
        .map(|d| {
            let pos = ((d as f32 + 0.5) * src_len as f32 / dst_len as f32 - 0.5).max(0.0);
            let i0 = (pos as usize).min(src_len - 1);
            let i1 = (i0 + 1).min(src_len - 1);
            (i0, i1, ((pos - i0 as f32) * 256.0) as u32)
        })
        .collect()
}

/// Bilinear resize of a [0, 1] matte into 0..=255 alpha.
fn resize_f32_to_u8(src: &[f32], sw: usize, sh: usize, dst: &mut Vec<u8>, dw: usize, dh: usize) {
    dst.resize(dw * dh, 0);
    let (xs, ys) = (axis(sw, dw), axis(sh, dh));
    dst.par_chunks_mut(dw).zip(ys.par_iter()).for_each(|(row, &(y0, y1, wy))| {
        let (r0, r1) = (&src[y0 * sw..(y0 + 1) * sw], &src[y1 * sw..(y1 + 1) * sw]);
        let fy = wy as f32 / 256.0;
        for (out, &(x0, x1, wx)) in row.iter_mut().zip(&xs) {
            let fx = wx as f32 / 256.0;
            let top = r0[x0] + (r0[x1] - r0[x0]) * fx;
            let bottom = r1[x0] + (r1[x1] - r1[x0]) * fx;
            *out = ((top + (bottom - top) * fy).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_endpoints_and_midpoint() {
        assert_eq!(mix(200, 10, 255), 200);
        assert_eq!(mix(200, 10, 0), 10);
        assert_eq!(mix(200, 0, 128), 100);
    }

    #[test]
    fn box_blur_keeps_flat_planes_flat_and_spreads_spikes() {
        let (w, h) = (16, 8);
        let mut flat = vec![0.3f32; w * h];
        let mut scratch = Vec::new();
        box_blur(&mut flat, w, h, 3, &mut scratch);
        assert!(flat.iter().all(|&v| (v - 0.3).abs() < 1e-5));

        let mut spike = vec![0.0f32; w * h];
        spike[4 * w + 8] = 9.0;
        box_blur(&mut spike, w, h, 1, &mut scratch);
        assert!((spike[4 * w + 8] - 1.0).abs() < 1e-5);
        assert!((spike[3 * w + 7] - 1.0).abs() < 1e-5);
        assert_eq!(spike[0], 0.0);
    }

    #[test]
    fn resize_matte_hits_ramp_ends() {
        let matte: Vec<f32> = (0..8).map(|x| x as f32 / 7.0).collect();
        let mut alpha = Vec::new();
        resize_f32_to_u8(&matte, 8, 1, &mut alpha, 64, 2);
        assert_eq!(alpha[0], 0);
        assert_eq!(alpha[63], 255);
    }

    #[test]
    fn masked_blur_does_not_bleed_person_into_background() {
        // Left half: bright person (alpha 255). Right half: dark background.
        let (w, h) = (64, 32);
        let mut src = vec![0u8; w * h];
        let mut alpha = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w / 2 {
                src[y * w + x] = 250;
                alpha[y * w + x] = 255;
            }
        }
        let mut buffers = BlurBuffers::default();
        let mut masked = Vec::new();
        masked_blur(&src, &alpha, w, h, 3, &mut buffers, &mut masked);

        // Just right of the boundary, a plain blur would be ~half bright; the
        // masked blur keeps the background dark.
        let next_to_person = masked[16 * w + w / 2 + 2];
        assert!(next_to_person < 20, "background picked up {next_to_person}");
        // Far from the person the background is untouched. (Values deep inside
        // the person don't matter: alpha 255 shows the frame there.)
        assert!(masked[16 * w + w - 2] < 5);

        // For contrast, an unmasked blur of the same frame does bleed.
        let mut plain = Vec::new();
        masked_blur(&src, &vec![0u8; w * h], w, h, 3, &mut buffers, &mut plain);
        assert!(plain[16 * w + w / 2 + 2] > 60, "plain blur gave {}", plain[16 * w + w / 2 + 2]);
    }

    #[test]
    fn smoothing_follows_motion_but_damps_noise() {
        let mut smoothed = vec![0.0f32, 0.0];
        smooth_matte(&mut smoothed, &[1.0, 0.1]);
        assert_eq!(smoothed[0], 1.0); // big change: taken immediately
        assert!((smoothed[1] - 0.05).abs() < 1e-6); // small change: averaged
    }

    #[test]
    fn smoothstep_saturates_outside_range() {
        assert_eq!(smoothstep(0.2, 0.6, 0.1), 0.0);
        assert_eq!(smoothstep(0.2, 0.6, 0.7), 1.0);
        assert!((smoothstep(0.2, 0.6, 0.4) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn replacement_image_covers_frame() {
        let mut img = image::RgbImage::new(40, 10);
        for (x, _, px) in img.enumerate_pixels_mut() {
            *px = if x < 20 { image::Rgb([255, 255, 255]) } else { image::Rgb([0, 0, 0]) };
        }
        let planes = image_to_planes(&img, 16, 16);
        assert_eq!(planes.y.len(), 256);
        assert_eq!(planes.y[0], 235); // left half white
        assert_eq!(planes.y[15], 16); // right half black
    }
}
