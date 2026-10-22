//! `vcam-app`: the Rust VCam command-line tool. Creates the "Rust VCam"
//! virtual camera, feeds it from a webcam, and keeps it alive until Enter is
//! pressed. (`rust-vcam` is the settings and tray app.)
//!
//! The media source DLL (`rust_vcam_source.dll`) must already be registered
//! with `scripts/install.ps1`. The camera uses system lifetime: it stays
//! registered after this process exits (clients then see the idle pattern), so
//! its Windows settings stick between runs. `--remove-camera` unregisters it.
//!
//! Usage:
//!   vcam-app                   capture from the default webcam until Enter is pressed
//!   vcam-app --camera NAME     capture from the first webcam whose name contains NAME
//!   vcam-app --pattern         send the animated test pattern instead of a webcam
//!   vcam-app --blur [N]        blur the background (strength 1-20, default 8)
//!   vcam-app --background IMG  replace the background with a JPEG/PNG image
//!   vcam-app --model PATH      segmentation model (default: the first known model in models\)
//!   vcam-app --cpu             run segmentation on the CPU instead of DirectML
//!   vcam-app --show-mask       debug: show the segmentation mask (white = person) instead of an effect
//!   vcam-app --no-publish      keep the camera up without sending frames (idle pattern)
//!   vcam-app --seconds N       stop after N seconds instead of waiting for Enter
//!   vcam-app --list-cameras    list webcams and their native modes
//!   vcam-app --bench-effects   time the effect pipeline on synthetic 720p frames (no camera needed)
//!   vcam-app --probe [N]       open the running camera as a client and read N frames (default 90)
//!   vcam-app --remove-camera   unregister the "Rust VCam" device (and reset its Windows settings)

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows_core::Result;

use vcam_app::controller::{self, InstanceGuard};
use vcam_app::effects::{self, Effect, Effects};
use vcam_app::publisher::{self, Config, Control, Feed};
use vcam_app::settings::DEFAULT_BLUR;
use vcam_app::{capture, devices, probe, segment, FRIENDLY_NAME};

struct RunOptions {
    seconds: Option<u64>,
    /// `None` with `--no-publish`.
    feed: Option<Feed>,
    effect: Option<Effect>,
    model: Option<PathBuf>,
    allow_gpu: bool,
    /// `--bench-effects`: time the effect pipeline on synthetic frames.
    bench: bool,
}

enum Mode {
    Run(RunOptions),
    ListCameras,
    Probe { frames: u32 },
    RemoveCamera,
}

const USAGE: &str = "usage: vcam-app [--camera NAME | --pattern | --no-publish] [--blur [N] | --background IMAGE | --show-mask]\n                [--model PATH] [--cpu] [--seconds N] [--bench-effects]\n       vcam-app --list-cameras\n       vcam-app --probe [FRAMES]\n       vcam-app --remove-camera";

fn usage_error(message: &str) -> ! {
    eprintln!("{message}\n{USAGE}");
    std::process::exit(2);
}

fn parse_args() -> Mode {
    let mut args = std::env::args().skip(1).peekable();
    let mut options =
        RunOptions { seconds: None, feed: Some(Feed::Camera(None)), effect: None, model: None, allow_gpu: true, bench: false };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--probe" => return Mode::Probe { frames: args.next().and_then(|v| v.parse().ok()).unwrap_or(90) },
            "--list-cameras" => return Mode::ListCameras,
            "--remove-camera" => return Mode::RemoveCamera,
            "--seconds" => {
                options.seconds =
                    Some(args.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage_error("--seconds needs a number")))
            }
            "--camera" => {
                options.feed = Some(Feed::Camera(Some(args.next().unwrap_or_else(|| usage_error("--camera needs a name")))))
            }
            "--pattern" => options.feed = Some(Feed::Pattern),
            "--no-publish" => options.feed = None,
            "--blur" => {
                // The strength is optional: take the next argument only if it's a number.
                let strength = args.next_if(|v| v.parse::<u32>().is_ok()).map_or(DEFAULT_BLUR, |v| v.parse().unwrap());
                if !(1..=20).contains(&strength) {
                    usage_error("--blur strength must be 1-20");
                }
                options.effect = Some(Effect::Blur { strength });
            }
            "--background" => {
                let image = args.next().unwrap_or_else(|| usage_error("--background needs an image path"));
                options.effect = Some(Effect::Replace { image: PathBuf::from(image) });
            }
            "--model" => options.model = Some(PathBuf::from(args.next().unwrap_or_else(|| usage_error("--model needs a path")))),
            "--cpu" => options.allow_gpu = false,
            "--show-mask" => options.effect = Some(Effect::ShowMask),
            "--bench-effects" => options.bench = true,
            other => usage_error(&format!("unknown argument: {other}")),
        }
    }
    Mode::Run(options)
}

/// Resolves the model and checks the effect's inputs before starting, so bad
/// paths fail fast. Returns `None` when no effect is selected.
fn resolve_model(options: &RunOptions) -> Option<PathBuf> {
    let effect = options.effect.as_ref()?;
    if !matches!(options.feed, Some(Feed::Camera(_))) {
        println!("Note: background effects apply to webcam frames only.");
    }
    if let Effect::Replace { image } = effect {
        if let Err(err) = effects::load_image(image) {
            eprintln!("{err}");
            std::process::exit(2);
        }
    }
    let model = options.model.clone().or_else(|| segment::default_model_path(options.allow_gpu)).unwrap_or_else(|| {
        let files: Vec<&str> = segment::KNOWN_MODELS.iter().map(|(file, _)| *file).collect();
        eprintln!("Segmentation model not found. Put {} in the models folder or pass --model PATH.", files.join(" or "));
        std::process::exit(2);
    });
    Some(model)
}

fn main() {
    vcam_app::init_thread_pool();
    if let Err(err) = run(parse_args()) {
        eprintln!("error: {err} ({:#010x})", err.code().0);
        std::process::exit(1);
    }
}

fn run(mode: Mode) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }

    let result = match mode {
        Mode::Run(options) if options.bench => bench_effects(options),
        Mode::Run(options) => run_camera(options),
        Mode::ListCameras => list_cameras(),
        Mode::RemoveCamera => remove_camera(),
        Mode::Probe { frames } => probe::probe(FRIENDLY_NAME, frames),
    };

    unsafe {
        let _ = MFShutdown();
        CoUninitialize();
    }
    result
}

fn list_cameras() -> Result<()> {
    let devices = devices::video_devices()?;
    if devices.is_empty() {
        println!("No video capture devices found.");
    }
    let default = devices::pick_camera(devices::video_devices()?, None).map(|d| d.name);
    for device in &devices {
        let marker = if Some(&device.name) == default.as_ref() { " (default)" } else { "" };
        println!("{}{marker}", device.name);
        if device.name.starts_with(devices::OWN_CAMERA_PREFIX) {
            continue;
        }
        match capture::native_modes(device) {
            Ok(modes) => modes.iter().for_each(|mode| println!("    {mode}")),
            Err(err) => println!("    could not read modes: {err}"),
        }
    }
    Ok(())
}

fn run_camera(options: RunOptions) -> Result<()> {
    let Some(_guard) = InstanceGuard::acquire() else {
        eprintln!("Rust VCam is already running (check the system tray). Quit it first.");
        std::process::exit(2);
    };
    let model = resolve_model(&options);
    let camera = controller::create_virtual_camera().inspect_err(|err| eprintln!("{}", controller::describe_camera_error(err)))?;
    println!("\"{FRIENDLY_NAME}\" is running. Open the Camera app, Teams or Zoom and select it.");

    let publisher = options.feed.clone().map(|feed| {
        let config = Config { feed, effect: options.effect.clone(), model, allow_gpu: options.allow_gpu };
        let control = Arc::new(Control::new(config, true));
        let thread = {
            let control = Arc::clone(&control);
            std::thread::spawn(move || {
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                publisher::run(&control);
                if com.is_ok() {
                    unsafe { CoUninitialize() };
                }
            })
        };
        (control, thread)
    });
    if publisher.is_none() {
        println!("Not publishing frames (--no-publish); clients see the idle pattern.");
    }

    match options.seconds {
        Some(seconds) => {
            println!("Stopping in {seconds} s.");
            std::thread::sleep(Duration::from_secs(seconds));
        }
        None => {
            println!("Press Enter to stop.");
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
        }
    }

    if let Some((control, thread)) = publisher {
        control.stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
    }
    controller::release_virtual_camera(&camera);
    println!("Stopped. \"{FRIENDLY_NAME}\" stays installed (idle pattern); vcam-app --remove-camera removes it.");
    Ok(())
}

fn remove_camera() -> Result<()> {
    let Some(_guard) = InstanceGuard::acquire() else {
        eprintln!("Rust VCam is running (check the system tray). Quit it first.");
        std::process::exit(2);
    };
    controller::unregister_virtual_camera().inspect_err(|err| eprintln!("{}", controller::describe_camera_error(err)))?;
    println!("Removed \"{FRIENDLY_NAME}\".");
    Ok(())
}

/// Runs the effect pipeline on a synthetic, moving 1280x720 NV12 test pattern
/// at camera pace (30 fps) and prints per-stage timings every 5 seconds.
fn bench_effects(mut options: RunOptions) -> Result<()> {
    use vcam_shared::pattern::{Style, TestPattern};
    use vcam_shared::{FrameFormat, PixelFormat};

    let effect = options.effect.get_or_insert(Effect::Blur { strength: DEFAULT_BLUR }).clone();
    options.feed = Some(Feed::Camera(None));
    let Some(model) = resolve_model(&options) else { return Ok(()) };
    let mut effects = match Effects::new(effect, model, options.allow_gpu) {
        Ok(effects) => effects,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    };
    let seconds = options.seconds.unwrap_or(15);
    println!("Benchmarking {} for {seconds} s on synthetic 1280x720 NV12 frames at 30 fps.", effects.effect());

    let format = FrameFormat { pixel: PixelFormat::Nv12, width: 1280, height: 720 };
    let mut pattern = TestPattern::new(Style::Live);
    let mut frame = vec![0u8; format.frame_bytes()];
    let interval = Duration::from_nanos(1_000_000_000 / 30);
    let started = Instant::now();
    let mut next = started;
    let mut report_at = started + Duration::from_secs(5);
    let mut frames = 0u32;
    let mut busy = Duration::ZERO;
    while started.elapsed() < Duration::from_secs(seconds) {
        frame.resize(format.frame_bytes(), 0);
        pattern.render(&format, &mut frame);
        let work = Instant::now();
        effects.process(format, &mut frame);
        busy += work.elapsed();
        frames += 1;

        let now = Instant::now();
        if now >= report_at {
            let (inferences, inference_ms) = effects.take_stats();
            let backend = effects.backend().map_or("(loading)".to_string(), |b| b.to_string());
            let stages = effects
                .take_timings()
                .map_or(String::new(), |(p, w, c)| format!("; prepare {p:.1} ms, wait {w:.1} ms, composite {c:.1} ms"));
            println!(
                "process() {:.1} ms/frame (budget 33.3){stages}; segmentation {inferences} runs at {inference_ms:.1} ms on {backend}.",
                busy.as_secs_f64() * 1000.0 / frames.max(1) as f64
            );
            (frames, busy) = (0, Duration::ZERO);
            report_at = now + Duration::from_secs(5);
        }
        next += interval;
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now;
        }
    }
    Ok(())
}
