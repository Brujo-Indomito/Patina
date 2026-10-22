//! Publishes frames to the media source over shared memory.
//!
//! The webcam runs only while it's needed: while a client watches the virtual
//! camera (the source's heartbeat is fresh), in the format that client
//! negotiated, or while the settings window shows a preview. Otherwise it is
//! closed and the camera light is off. If the webcam can't be opened, the test
//! pattern is sent instead and the webcam is retried periodically.
//!
//! Everything the publisher needs from outside goes through [`Control`]:
//! configuration that can change at any time (camera, effect, model), a status
//! snapshot for the UI, preview frames, and the stop flag. The command-line
//! mode uses the same path with `print_log` on.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use patina_shared::pattern::{Style, TestPattern};
use patina_shared::shm::{now_ms, Producer};
use patina_shared::{FrameFormat, PixelFormat};

use crate::capture::Camera;
use crate::devices;
use crate::effects::{Effect, Effects};
use crate::segment;

const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);
const CONNECT_RETRY: Duration = Duration::from_millis(250);
const IDLE_POLL: Duration = Duration::from_millis(100);
const CAMERA_RETRY: Duration = Duration::from_secs(5);
const RATE_REPORT: Duration = Duration::from_secs(5);
/// Unload the segmentation model after this long without a client or preview.
/// Most of its memory is the GPU device, ~300 MB that an idle tray app
/// shouldn't hold; reloading takes about a second.
const EFFECTS_IDLE_UNLOAD: Duration = Duration::from_secs(30);
/// Format used for the preview while no client is watching.
const PREVIEW_FORMAT: FrameFormat = FrameFormat { pixel: PixelFormat::Nv12, width: 1280, height: 720 };
/// Preview frames are scaled down to this size for the UI.
pub const PREVIEW_WIDTH: usize = 640;
pub const PREVIEW_HEIGHT: usize = 360;
/// Lines kept in the activity log.
const LOG_LINES: usize = 50;

/// What to send to the virtual camera.
#[derive(Clone, Debug, PartialEq)]
pub enum Feed {
    /// A webcam: the first whose name contains the string, or the default pick.
    Camera(Option<String>),
    /// The animated test pattern.
    Pattern,
}

/// Settings the publisher applies live.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub feed: Feed,
    /// Background effect for webcam frames, or `None`.
    pub effect: Option<Effect>,
    /// Segmentation model; `None` picks the default.
    pub model: Option<PathBuf>,
    pub allow_gpu: bool,
}

/// Snapshot of what the publisher is doing, for the UI.
#[derive(Clone, Debug, Default)]
pub struct Status {
    /// Format a client is consuming, if one is watching.
    pub client: Option<FrameFormat>,
    /// Whether frames are currently being produced for the preview only.
    pub previewing: bool,
    /// Webcam name and native mode while capturing.
    pub camera: Option<String>,
    pub fps: f64,
    pub new_image_fps: f64,
    /// For example "RVM on DirectML (GPU)", once the model has loaded.
    pub segmentation: Option<String>,
    pub segmentation_rate: f64,
    pub segmentation_ms: f64,
    /// Per-frame (prepare, wait, composite) milliseconds.
    pub timings: Option<(f64, f64, f64)>,
    pub coverage: Option<(f32, f32)>,
    /// Most recent problem with the webcam or the effect, if any.
    pub problem: Option<String>,
    pub log: Vec<String>,
}

/// A scaled-down RGBA copy of the frame being sent.
pub struct Preview {
    pub sequence: u64,
    pub rgba: Vec<u8>,
}

pub struct Control {
    pub stop: AtomicBool,
    config: Mutex<(u64, Config)>,
    status: Mutex<Status>,
    preview: Mutex<Option<Preview>>,
    /// The UI refreshes this while it shows the preview (a `now_ms` tick).
    preview_wanted_until: AtomicU64,
    /// Echo log lines to stdout (command-line mode).
    print_log: bool,
}

impl Control {
    pub fn new(config: Config, print_log: bool) -> Self {
        Self {
            stop: AtomicBool::new(false),
            config: Mutex::new((0, config)),
            status: Mutex::new(Status::default()),
            preview: Mutex::new(None),
            preview_wanted_until: AtomicU64::new(0),
            print_log,
        }
    }

    pub fn set_config(&self, config: Config) {
        let mut guard = lock(&self.config);
        if guard.1 != config {
            *guard = (guard.0 + 1, config);
        }
    }

    fn config(&self) -> (u64, Config) {
        lock(&self.config).clone()
    }

    pub fn status(&self) -> Status {
        lock(&self.status).clone()
    }

    fn update_status(&self, f: impl FnOnce(&mut Status)) {
        f(&mut lock(&self.status));
    }

    /// Asks for preview frames for the next second.
    pub fn want_preview(&self) {
        self.preview_wanted_until.store(now_ms() + 1000, Ordering::Relaxed);
    }

    fn preview_wanted(&self) -> bool {
        now_ms() < self.preview_wanted_until.load(Ordering::Relaxed)
    }

    /// Takes the newest preview frame if it's newer than `after`.
    pub fn take_preview(&self, after: u64) -> Option<Preview> {
        let mut slot = lock(&self.preview);
        match slot.as_ref() {
            Some(preview) if preview.sequence > after => slot.take(),
            _ => None,
        }
    }

    pub fn log(&self, line: String) {
        if self.print_log {
            println!("{line}");
        }
        self.update_status(|status| {
            status.log.push(line);
            let excess = status.log.len().saturating_sub(LOG_LINES);
            status.log.drain(..excess);
        });
    }

    fn problem(&self, line: String) {
        self.update_status(|status| status.problem = Some(line.clone()));
        self.log(line);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs until `control.stop` is set.
pub fn run(control: &Control) {
    let mut producer: Option<Producer> = None;
    let mut next_connect = Instant::now();
    let mut pattern = TestPattern::new(Style::Live);
    let mut active_format: Option<FrameFormat> = None;
    let mut camera: Option<Camera> = None;
    let mut camera_retry_at = Instant::now();
    let mut effects: Option<Effects> = None;
    let mut applied: Option<(u64, Config)> = None;
    let mut frame = Vec::new();
    let mut next_frame = Instant::now();
    let mut frames_sent = 0u64;
    let mut preview_sequence = 0u64;
    let mut stats = RateStats::new();
    // When frames stopped being needed, to unload the model after a while.
    let mut idle_since: Option<Instant> = None;
    // Set when loading the effect failed for the current configuration.
    let mut effects_failed = false;

    while !control.stop.load(Ordering::Relaxed) {
        // Apply configuration changes.
        let (version, config) = control.config();
        if applied.as_ref().map(|(v, _)| *v) != Some(version) {
            apply_config(control, applied.as_ref().map(|(_, c)| c), &config, &mut camera, &mut effects);
            camera_retry_at = Instant::now();
            effects_failed = false;
            applied = Some((version, config.clone()));
        }

        // Connect to the source's shared memory once it exists.
        if producer.is_none() && Instant::now() >= next_connect {
            match Producer::open() {
                Ok(link) => {
                    control.log("Connected to the camera source.".into());
                    producer = Some(link);
                }
                // The source creates the section when a client opens the camera.
                Err(_) => next_connect = Instant::now() + CONNECT_RETRY,
            }
        }
        if let Some(link) = &producer {
            link.heartbeat();
        }

        let client = producer.as_ref().and_then(Producer::requested);
        let previewing = client.is_none() && control.preview_wanted();
        let Some(format) = client.or(previewing.then_some(PREVIEW_FORMAT)) else {
            if active_format.take().is_some() {
                control.log(format!("No client watching; idle ({frames_sent} frames sent so far)."));
                control.update_status(|s| {
                    (s.client, s.previewing, s.camera, s.fps, s.new_image_fps) = (None, false, None, 0.0, 0.0);
                });
            }
            if camera.take().is_some() {
                control.log("Webcam closed.".into());
            }
            // Release the model (and its GPU device) once idle for a while.
            let idle_for = idle_since.get_or_insert_with(Instant::now).elapsed();
            if effects.is_some() && idle_for >= EFFECTS_IDLE_UNLOAD {
                unload_effects(control, &mut effects);
                control.log("Unloaded the segmentation model while idle.".into());
            }
            std::thread::sleep(IDLE_POLL);
            continue;
        };
        idle_since = None;
        control.update_status(|s| (s.client, s.previewing) = (client, previewing));
        if matches!(config.feed, Feed::Camera(_)) && !effects_failed {
            effects_failed = !ensure_effects(control, &config, &mut effects);
        }

        if active_format != Some(format) {
            control.log(match client {
                Some(_) => format!("Client is watching: sending {format}."),
                None => format!("Previewing at {format}."),
            });
            active_format = Some(format);
            camera = None; // reopen for the new format
            camera_retry_at = Instant::now();
            next_frame = Instant::now();
        }

        if let Feed::Camera(wanted) = &config.feed {
            if camera.is_none() && Instant::now() >= camera_retry_at {
                match open_camera(wanted.as_deref(), format) {
                    Ok(opened) => {
                        let description = format!("{} ({})", opened.name(), opened.mode());
                        control.log(format!("Capturing from \"{}\" ({}) -> {}.", opened.name(), opened.mode(), opened.output()));
                        control.update_status(|s| {
                            s.camera = Some(description);
                            s.problem = None;
                        });
                        camera = Some(opened);
                        stats = RateStats::new();
                    }
                    Err(err) => {
                        control.problem(format!(
                            "Webcam unavailable ({err}); sending the test pattern, retrying in {} s.",
                            CAMERA_RETRY.as_secs()
                        ));
                        camera_retry_at = Instant::now() + CAMERA_RETRY;
                    }
                }
            }
        }

        let produced = if let Some(active) = &camera {
            // The camera paces the loop: read_frame blocks until the next frame.
            match active.read_frame(&mut frame) {
                Ok(true) => {
                    stats.count(&frame);
                    if let Some(effects) = effects.as_mut() {
                        effects.process(format, &mut frame);
                    }
                    true
                }
                Ok(false) => false,
                Err(err) => {
                    control.problem(format!("Webcam stopped ({err}); sending the test pattern."));
                    camera = None;
                    control.update_status(|s| s.camera = None);
                    camera_retry_at = Instant::now() + CAMERA_RETRY;
                    false
                }
            }
        } else {
            frame.resize(format.frame_bytes(), 0);
            pattern.render(&format, &mut frame);
            let now = Instant::now();
            next_frame += FRAME_INTERVAL;
            if next_frame > now {
                std::thread::sleep(next_frame - now);
            } else {
                next_frame = now;
            }
            true
        };
        if !produced {
            continue;
        }

        if client.is_some() {
            if let Some(link) = producer.as_mut() {
                link.write_frame(format, |dst| dst.copy_from_slice(&frame));
            }
            frames_sent += 1;
        }
        if control.preview_wanted() && format.pixel == PixelFormat::Nv12 {
            preview_sequence += 1;
            let rgba = nv12_to_preview(&frame, format.width as usize, format.height as usize);
            *lock(&control.preview) = Some(Preview { sequence: preview_sequence, rgba });
        }

        if camera.is_some() && stats.due() {
            stats.report(control, effects.as_mut());
        }
    }
}

/// Rebuilds whatever a configuration change affects.
fn apply_config(control: &Control, old: Option<&Config>, new: &Config, camera: &mut Option<Camera>, effects: &mut Option<Effects>) {
    if old.map(|c| &c.feed) != Some(&new.feed) {
        *camera = None; // reopened with the new feed on the next frame
        control.update_status(|s| s.camera = None);
    }
    // The model itself is loaded lazily by `ensure_effects` when frames need it.
    let model_changed = old.is_some_and(|c| c.model != new.model || c.allow_gpu != new.allow_gpu);
    match &new.effect {
        None => {
            if old.is_some_and(|c| c.effect.is_some()) {
                control.log("Effect: none.".into());
            }
            unload_effects(control, effects);
        }
        Some(effect) => {
            if model_changed {
                unload_effects(control, effects);
            }
            if let Some(existing) = effects.as_mut() {
                if let Err(err) = existing.set_effect(effect.clone()) {
                    control.problem(err);
                    unload_effects(control, effects);
                }
            }
            control.log(format!("Effect: {effect}."));
        }
    }
}

/// Loads the effect pipeline if the configuration wants one and it isn't
/// loaded. Returns false if loading failed (reported once per configuration).
fn ensure_effects(control: &Control, config: &Config, effects: &mut Option<Effects>) -> bool {
    let Some(effect) = &config.effect else { return true };
    if effects.is_some() {
        return true;
    }
    match create_effects(config, effect) {
        Ok(created) => {
            control.log("Loading the segmentation model.".into());
            *effects = Some(created);
            true
        }
        Err(err) => {
            control.problem(err);
            false
        }
    }
}

/// Drops the effect pipeline, releasing the model and its GPU device.
fn unload_effects(control: &Control, effects: &mut Option<Effects>) {
    if effects.take().is_some() {
        control.update_status(|s| (s.segmentation, s.timings, s.coverage) = (None, None, None));
    }
}

fn create_effects(config: &Config, effect: &Effect) -> Result<Effects, String> {
    let model = config.model.clone().or_else(|| segment::default_model_path(config.allow_gpu)).ok_or_else(|| {
        let files: Vec<&str> = segment::KNOWN_MODELS.iter().map(|(file, _)| *file).collect();
        format!("Segmentation model not found. Put {} in the models folder.", files.join(" or "))
    })?;
    Effects::new(effect.clone(), model, config.allow_gpu)
}

/// Frame-rate and effect statistics, reported every RATE_REPORT.
struct RateStats {
    since: Instant,
    frames: u32,
    new_images: u32,
    previous_fingerprint: u64,
}

impl RateStats {
    fn new() -> Self {
        Self { since: Instant::now(), frames: 0, new_images: 0, previous_fingerprint: 0 }
    }

    fn count(&mut self, frame: &[u8]) {
        let fingerprint = fingerprint(frame);
        if fingerprint != self.previous_fingerprint {
            self.new_images += 1;
        }
        self.previous_fingerprint = fingerprint;
        self.frames += 1;
    }

    fn due(&self) -> bool {
        self.since.elapsed() >= RATE_REPORT
    }

    fn report(&mut self, control: &Control, effects: Option<&mut Effects>) {
        let seconds = self.since.elapsed().as_secs_f64();
        let (fps, new_image_fps) = (self.frames as f64 / seconds, self.new_images as f64 / seconds);
        let mut report = format!("Webcam delivering {fps:.1} fps ({new_image_fps:.1} fps of new images)");
        control.update_status(|s| (s.fps, s.new_image_fps) = (fps, new_image_fps));

        if let Some(effects) = effects {
            let (inferences, inference_ms) = effects.take_stats();
            let rate = inferences as f64 / seconds;
            let segmentation = effects.description();
            report += &format!(
                "; segmentation {rate:.1}/s at {inference_ms:.1} ms on {}",
                effects.backend().map_or("(loading)".to_string(), |b| b.to_string())
            );
            let timings = effects.take_timings();
            if let Some((prepare, wait, composite)) = timings {
                report += &format!("; per frame: prepare {prepare:.1} ms, wait {wait:.1} ms, composite {composite:.1} ms");
            }
            if let Some((mean, max)) = effects.take_lag() {
                report += &format!("; mask lag {mean:.1} frames (max {max})");
            }
            let coverage = effects.take_coverage_range();
            if let Some((low, high)) = coverage {
                report += &format!("; person covers {:.0}-{:.0}% of the frame", low * 100.0, high * 100.0);
            }
            control.update_status(|s| {
                s.segmentation = segmentation;
                (s.segmentation_rate, s.segmentation_ms, s.timings, s.coverage) = (rate, inference_ms, timings, coverage);
            });
        }
        control.log(format!("{report}."));
        *self = Self { previous_fingerprint: self.previous_fingerprint, ..Self::new() };
    }
}

/// Cheap content fingerprint (FNV-1a over every 61st byte) to count new images.
fn fingerprint(frame: &[u8]) -> u64 {
    frame.iter().step_by(61).fold(0xcbf2_9ce4_8422_2325u64, |hash, &b| (hash ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Scales an NV12 frame down to PREVIEW_WIDTH x PREVIEW_HEIGHT RGBA
/// (nearest neighbor).
fn nv12_to_preview(frame: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut rgba = vec![255u8; PREVIEW_WIDTH * PREVIEW_HEIGHT * 4];
    let uv_plane = &frame[width * height..];
    rgba.par_chunks_mut(PREVIEW_WIDTH * 4).enumerate().for_each(|(py, row)| {
        let y = py * height / PREVIEW_HEIGHT;
        let luma_row = &frame[y * width..(y + 1) * width];
        let chroma_row = &uv_plane[(y / 2) * width..(y / 2) * width + width];
        for (px, out) in row.chunks_exact_mut(4).enumerate() {
            let x = px * width / PREVIEW_WIDTH;
            let [r, g, b] = segment::yuv_to_rgb(luma_row[x], chroma_row[x & !1], chroma_row[x | 1]);
            out[..3].copy_from_slice(&[r, g, b]);
        }
    });
    rgba
}

fn open_camera(wanted: Option<&str>, format: FrameFormat) -> windows_core::Result<Camera> {
    let device = devices::pick_camera(devices::video_devices()?, wanted)
        .ok_or_else(|| windows_core::Error::new(windows::Win32::Media::MediaFoundation::MF_E_NOT_FOUND, "no matching webcam"))?;
    Camera::open(device, format)
}
