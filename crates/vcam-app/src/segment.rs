//! Person segmentation on ONNX Runtime.
//!
//! Two model families are supported and told apart by their inputs:
//!
//! - **RVM** (Robust Video Matting, MobileNetV3; GPL-3.0): built for webcam
//!   video. It takes the frame as RGB in [0, 1] plus four recurrent state
//!   tensors carried over from the previous frame and a downsample ratio, and
//!   returns an alpha matte (`pha`) and the updated states. The recurrence is
//!   what makes it temporally stable and keeps arms and hands.
//! - **MODNet** (Apache-2.0): a single-image portrait matting model taking RGB
//!   in [-1, 1] and returning the matte.
//!
//! Inference runs on its own thread. The publisher hands over each frame and
//! either waits briefly for that frame's matte (GPU) or uses the newest one
//! (CPU), so a slow model lowers how often the mask updates but never the
//! camera frame rate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use half::f16;
use half::slice::HalfFloatSliceExt;
use ort::ep;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::{Shape, TensorElementType, TensorRef, ValueType};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelKind {
    Rvm,
    Modnet,
}

impl ModelKind {
    /// Model input (and matte) size; 16:9 like the camera's output formats.
    pub fn input_size(self) -> (usize, usize) {
        match self {
            // RVM refines its low-resolution pass (480x270) back up to the input size.
            ModelKind::Rvm => (960, 540),
            // MODNet needs multiples of 32.
            ModelKind::Modnet => (512, 288),
        }
    }

    fn normalize(self, value: u8) -> f32 {
        match self {
            ModelKind::Rvm => value as f32 / 255.0,
            ModelKind::Modnet => value as f32 / 127.5 - 1.0,
        }
    }
}

impl std::fmt::Display for ModelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ModelKind::Rvm => "RVM",
            ModelKind::Modnet => "MODNet",
        })
    }
}

/// RVM's downsample ratio: its low-resolution pass runs at 480x270, the
/// documented recommendation for 720p portrait video ("between 256 and 512").
const RVM_DOWNSAMPLE_RATIO: f32 = 0.5;

/// Known model files with their SHA-256. RVM files come from
/// github.com/PeterL1n/RobustVideoMatting release v1.0.0 (GPL-3.0); MODNet is
/// onnx/model.onnx from huggingface.co/Xenova/modnet (Apache-2.0).
pub const RVM_FP16: &str = "rvm_mobilenetv3_fp16.onnx";
pub const RVM_FP32: &str = "rvm_mobilenetv3_fp32.onnx";
pub const MODNET: &str = "modnet.onnx";
pub const KNOWN_MODELS: [(&str, &str); 3] = [
    (RVM_FP16, "6a0d5ce6cc17702613be548559879b4521ed424cfe14ddc48d1acaa44d616f64"),
    (RVM_FP32, "88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828"),
    (MODNET, "07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9"),
];

/// Finds RVM, preferring the fp16 export on the GPU (half the memory; GPUs
/// compute fp16 natively) and fp32 on the CPU (where fp16 is emulated and slow).
pub fn rvm_path(gpu: bool) -> Option<PathBuf> {
    let order = if gpu { [RVM_FP16, RVM_FP32] } else { [RVM_FP32, RVM_FP16] };
    order.into_iter().find_map(find_model)
}

/// Finds a known model, preferring RVM: in `models\` next to the executable,
/// then in the workspace's `models\` directory (for `cargo run`).
pub fn default_model_path(gpu: bool) -> Option<PathBuf> {
    rvm_path(gpu).or_else(|| find_model(MODNET))
}

/// Finds a model file by name in the same places as [`default_model_path`].
pub fn find_model(file: &str) -> Option<PathBuf> {
    let beside_exe = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("models")));
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("models");
    beside_exe.into_iter().chain([workspace]).map(|dir| dir.join(file)).find(|path| path.is_file())
}

/// Which execution provider ended up running the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    DirectMl,
    Cpu,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::DirectMl => "DirectML (GPU)",
            Backend::Cpu => "CPU",
        })
    }
}

/// RVM recurrent states r1..r4 from the previous frame (`None` = zeros), kept
/// in the model's own precision.
type States<T> = [Option<(Shape, Vec<T>)>; 4];

enum RvmStates {
    F32(States<f32>),
    F16(States<f16>),
}

struct Model {
    session: Session,
    backend: Backend,
    kind: ModelKind,
    states: RvmStates,
    /// Reused f16 copy of the input for fp16 models.
    half_input: Vec<f16>,
}

impl Model {
    fn load(path: &Path, allow_gpu: bool) -> Result<Self, Error> {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read model {}: {e}", path.display()))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if !KNOWN_MODELS.iter().any(|(_, sha)| *sha == digest) {
            println!("Note: {} is not a known model build (SHA-256 {digest}); using it anyway.", path.display());
        }

        let (session, backend) = Self::create_session(environment()?, &bytes, allow_gpu)?;
        let kind = if session.inputs().iter().any(|input| input.name() == "r1i") { ModelKind::Rvm } else { ModelKind::Modnet };
        let half = session
            .inputs()
            .first()
            .is_some_and(|input| matches!(input.dtype(), ValueType::Tensor { ty: TensorElementType::Float16, .. }));
        if half && kind == ModelKind::Modnet {
            return Err("fp16 MODNet models are not supported".into());
        }
        if half && backend == Backend::Cpu {
            println!("Note: running an fp16 model on the CPU is slow; the fp32 model is faster there.");
        }
        let states = if half { RvmStates::F16(Default::default()) } else { RvmStates::F32(Default::default()) };
        Ok(Self { session, backend, kind, states, half_input: Vec::new() })
    }

    /// Whether the model computes in half precision.
    fn is_half(&self) -> bool {
        matches!(self.states, RvmStates::F16(_))
    }

    /// "RVM fp16", "RVM fp32" or "MODNet".
    fn label(&self) -> String {
        match self.kind {
            ModelKind::Rvm => format!("RVM {}", if self.is_half() { "fp16" } else { "fp32" }),
            ModelKind::Modnet => ModelKind::Modnet.to_string(),
        }
    }

    fn create_session(environment: &ort::environment::Environment, bytes: &[u8], allow_gpu: bool) -> Result<(Session, Backend), Error> {
        if allow_gpu {
            let gpu = (|| -> Result<Session, Error> {
                let builder = Session::builder(environment)
                    .map_err(text)?
                    .with_optimization_level(GraphOptimizationLevel::Level3)
                    .map_err(text)?
                    // DirectML requires sequential execution without memory patterns.
                    .with_memory_pattern(false)
                    .map_err(text)?
                    .with_parallel_execution(false)
                    .map_err(text)?
                    .with_execution_providers([ep::DirectML::default().build().error_on_failure()])
                    .map_err(text)?;
                tune_memory(builder)?.commit_from_memory(bytes).map_err(text)
            })();
            match gpu {
                Ok(session) => return Ok((session, Backend::DirectMl)),
                Err(err) => println!("DirectML unavailable ({err}); running segmentation on the CPU."),
            }
        }
        let builder = Session::builder(environment)
            .map_err(text)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(text)?
            .with_intra_threads(2)
            .map_err(text)?;
        let session = tune_memory(builder)?.commit_from_memory(bytes).map_err(text)?;
        Ok((session, Backend::Cpu))
    }

    fn infer(&mut self, input: &[f32], matte: &mut Vec<f32>) -> Result<(), Error> {
        let (width, height) = self.kind.input_size();
        let src = TensorRef::from_array_view(([1usize, 3, height, width], input)).map_err(text)?;
        match self.kind {
            ModelKind::Modnet => {
                let outputs = self.session.run(ort::inputs![src]).map_err(text)?;
                let (_, data) = outputs[0].try_extract_tensor::<f32>().map_err(text)?;
                copy_matte(data, width * height, matte)
            }
            ModelKind::Rvm => {
                drop(src);
                let dims = [1usize, 3, height, width];
                match &mut self.states {
                    RvmStates::F32(states) => run_rvm(&mut self.session, states, dims, input, matte, |v| v),
                    RvmStates::F16(states) => {
                        self.half_input.resize(input.len(), f16::ZERO);
                        self.half_input.convert_from_f32_slice(input);
                        run_rvm(&mut self.session, states, dims, &self.half_input, matte, f16::to_f32)
                    }
                }
            }
        }
    }
}

/// Runs one RVM step in the model's precision `T`: feeds the frame and the
/// previous recurrent states, writes the matte as f32, and keeps the new states.
fn run_rvm<T>(
    session: &mut Session,
    states: &mut States<T>,
    dims: [usize; 4],
    src: &[T],
    matte: &mut Vec<f32>,
    to_f32: impl Fn(T) -> f32,
) -> Result<(), Error>
where
    T: ort::value::PrimitiveTensorElementType + Clone + Copy + Default + std::fmt::Debug + 'static,
{
    let zero = [T::default()];
    let ratio = [RVM_DOWNSAMPLE_RATIO];
    let state = |i: usize| match &states[i] {
        Some((shape, data)) => TensorRef::from_array_view((shape.clone(), data.as_slice())),
        None => TensorRef::from_array_view(([1usize, 1, 1, 1], &zero[..])),
    };
    let inputs = ort::inputs![
        "src" => TensorRef::from_array_view((dims, src)).map_err(text)?,
        "r1i" => state(0).map_err(text)?,
        "r2i" => state(1).map_err(text)?,
        "r3i" => state(2).map_err(text)?,
        "r4i" => state(3).map_err(text)?,
        "downsample_ratio" => TensorRef::from_array_view(([1usize], &ratio[..])).map_err(text)?,
    ];
    let outputs = session.run(inputs).map_err(text)?;
    let (_, alpha) = outputs["pha"].try_extract_tensor::<T>().map_err(text)?;
    let expected = dims[2] * dims[3];
    if alpha.len() != expected {
        return Err(format!("unexpected matte size {} (expected {expected})", alpha.len()).into());
    }
    matte.clear();
    matte.extend(alpha.iter().map(|&a| to_f32(a)));
    let mut next: States<T> = Default::default();
    for (i, slot) in next.iter_mut().enumerate() {
        let (shape, data) = outputs[format!("r{}o", i + 1).as_str()].try_extract_tensor::<T>().map_err(text)?;
        *slot = Some((shape.clone(), data.to_vec()));
    }
    drop(outputs);
    *states = next;
    Ok(())
}

/// The process-wide ONNX Runtime environment. ONNX Runtime allows only one per
/// process, and the app can briefly hold two models (when the model setting
/// changes, the new one loads before the old one is dropped).
fn environment() -> Result<&'static ort::environment::Environment, Error> {
    static ENVIRONMENT: OnceLock<ort::environment::Environment> = OnceLock::new();
    static INIT: Mutex<()> = Mutex::new(());
    if let Some(environment) = ENVIRONMENT.get() {
        return Ok(environment);
    }
    let _guard = INIT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(environment) = ENVIRONMENT.get() {
        return Ok(environment);
    }
    let environment = ort::init().with_name("rust-vcam").build().map_err(text)?;
    Ok(ENVIRONMENT.get_or_init(|| environment))
}

/// Keeps ONNX Runtime from holding on to peak-size CPU buffers: no CPU memory
/// arena, and weights allocated directly rather than through it. Measured
/// with RVM fp32 on the CPU, private memory went from 197 MB to 114 MB at the
/// same speed; on DirectML the gain is small (~2%), since most memory there
/// belongs to the GPU device.
fn tune_memory(mut builder: ort::session::builder::SessionBuilder) -> Result<ort::session::builder::SessionBuilder, Error> {
    use ort::AsPointer;
    let status = unsafe { (ort::api().DisableCpuMemArena)(builder.ptr_mut()) };
    if !status.0.is_null() {
        unsafe { (ort::api().ReleaseStatus)(status.0) };
        return Err("DisableCpuMemArena failed".into());
    }
    builder.with_config_entry("session.use_device_allocator_for_initializers", "1").map_err(text)
}

fn copy_matte(data: &[f32], expected: usize, matte: &mut Vec<f32>) -> Result<(), Error> {
    if data.len() != expected {
        return Err(format!("unexpected matte size {} (expected {expected})", data.len()).into());
    }
    matte.clear();
    matte.extend_from_slice(data);
    Ok(())
}

/// `ort` errors can carry non-`Send` state; keep only their message.
fn text(err: impl std::fmt::Display) -> Error {
    err.to_string().into()
}

/// Converts an NV12 frame into `kind`'s normalized RGB planes at its input
/// size (nearest-neighbor sampling; aliasing doesn't matter for matting).
pub fn nv12_to_input(frame: &[u8], width: usize, height: usize, kind: ModelKind, input: &mut Vec<f32>) {
    let (mw, mh) = kind.input_size();
    let pixels = mw * mh;
    input.resize(3 * pixels, 0.0);
    let (r_plane, rest) = input.split_at_mut(pixels);
    let (g_plane, b_plane) = rest.split_at_mut(pixels);
    let uv_plane = &frame[width * height..];

    r_plane
        .par_chunks_mut(mw)
        .zip(g_plane.par_chunks_mut(mw))
        .zip(b_plane.par_chunks_mut(mw))
        .enumerate()
        .for_each(|(my, ((r_row, g_row), b_row))| {
            let y = (my * height + height / 2) / mh;
            let luma_row = &frame[y * width..(y + 1) * width];
            let chroma_row = &uv_plane[(y / 2) * width..(y / 2) * width + width];
            for mx in 0..mw {
                let x = (mx * width + width / 2) / mw;
                let [r, g, b] = yuv_to_rgb(luma_row[x], chroma_row[x & !1], chroma_row[x | 1]);
                r_row[mx] = kind.normalize(r);
                g_row[mx] = kind.normalize(g);
                b_row[mx] = kind.normalize(b);
            }
        });
}

/// BT.601 limited-range YUV to RGB.
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = 298 * (y as i32 - 16);
    let (d, e) = (u as i32 - 128, v as i32 - 128);
    let clamp = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    [clamp(c + 409 * e), clamp(c - 100 * d - 208 * e), clamp(c + 516 * d)]
}

/// A matte and the number of the frame it was computed from.
pub struct Matte {
    pub frame: u64,
    pub width: usize,
    pub height: usize,
    /// `width` x `height` values in [0, 1].
    pub alpha: Vec<f32>,
}

#[derive(Default)]
struct Slots {
    /// Newest input (and its frame number) not yet picked up by the worker.
    input: Option<(u64, Vec<f32>)>,
    /// Spare buffer returned by the worker for reuse.
    spare: Option<Vec<f32>>,
    matte: Option<Arc<Matte>>,
    failed: Option<String>,
    /// Set once the model has loaded.
    loaded: Option<(Backend, ModelKind)>,
    /// For example "RVM fp16", once loaded.
    label: Option<String>,
}

struct Shared {
    slots: Mutex<Slots>,
    /// Signals the worker that input arrived (or that it should stop).
    wake: Condvar,
    /// Signals waiters that a new matte is ready.
    done: Condvar,
    stop: AtomicBool,
    inferences: AtomicU64,
    inference_micros: AtomicU64,
}

/// Runs the model on a background thread.
pub struct Segmenter {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    next_frame: AtomicU64,
}

impl Segmenter {
    /// Starts loading the model in the background; mattes become available
    /// once loading finishes.
    pub fn start(model_path: PathBuf, allow_gpu: bool) -> Self {
        let shared = Arc::new(Shared {
            slots: Mutex::new(Slots::default()),
            wake: Condvar::new(),
            done: Condvar::new(),
            stop: AtomicBool::new(false),
            inferences: AtomicU64::new(0),
            inference_micros: AtomicU64::new(0),
        });
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || worker(&shared, &model_path, allow_gpu))
        };
        Self { shared, worker: Some(worker), next_frame: AtomicU64::new(1) }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.shared.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Submits a frame for segmentation, replacing any frame still waiting,
    /// and returns its frame number. Frames submitted before the model has
    /// loaded are dropped.
    pub fn submit(&self, frame: &[u8], width: usize, height: usize) -> u64 {
        let number = self.next_frame.fetch_add(1, Ordering::Relaxed);
        let (kind, mut buffer) = {
            let mut slots = self.slots();
            let Some((_, kind)) = slots.loaded else { return number };
            (kind, slots.spare.take().unwrap_or_default())
        };
        nv12_to_input(frame, width, height, kind, &mut buffer);
        let mut slots = self.slots();
        if let Some((_, old)) = slots.input.replace((number, buffer)) {
            slots.spare = Some(old);
        }
        self.shared.wake.notify_one();
        number
    }

    /// The newest matte.
    pub fn matte(&self) -> Option<Arc<Matte>> {
        self.slots().matte.clone()
    }

    /// Waits up to `timeout` for the matte of frame `number` (or a later one),
    /// then returns the newest matte available.
    pub fn wait_for(&self, number: u64, timeout: Duration) -> Option<Arc<Matte>> {
        let deadline = Instant::now() + timeout;
        let mut slots = self.slots();
        loop {
            if slots.matte.as_ref().is_some_and(|m| m.frame >= number) || slots.failed.is_some() {
                return slots.matte.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return slots.matte.clone();
            }
            slots = self.shared.done.wait_timeout(slots, remaining).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
    }

    pub fn backend(&self) -> Option<Backend> {
        self.slots().loaded.map(|(backend, _)| backend)
    }

    pub fn kind(&self) -> Option<ModelKind> {
        self.slots().loaded.map(|(_, kind)| kind)
    }

    /// "RVM fp16", "RVM fp32" or "MODNet", once the model has loaded.
    pub fn label(&self) -> Option<String> {
        self.slots().label.clone()
    }

    /// Takes the load/inference error, if one happened since the last call.
    pub fn take_error(&self) -> Option<String> {
        self.slots().failed.take()
    }

    /// Returns (inferences, mean milliseconds) since the last call.
    pub fn take_stats(&self) -> (u64, f64) {
        let count = self.shared.inferences.swap(0, Ordering::Relaxed);
        let micros = self.shared.inference_micros.swap(0, Ordering::Relaxed);
        (count, if count > 0 { micros as f64 / count as f64 / 1000.0 } else { 0.0 })
    }
}

impl Drop for Segmenter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker(shared: &Shared, model_path: &Path, allow_gpu: bool) {
    let lock = || shared.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut model = match Model::load(model_path, allow_gpu) {
        Ok(model) => model,
        Err(err) => {
            lock().failed = Some(format!("could not load segmentation model: {err}"));
            shared.done.notify_all();
            return;
        }
    };
    let (width, height) = model.kind.input_size();
    {
        let mut slots = lock();
        slots.loaded = Some((model.backend, model.kind));
        slots.label = Some(model.label());
    }

    let mut matte = Vec::with_capacity(width * height);
    loop {
        let (number, input) = {
            let mut slots = lock();
            loop {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(input) = slots.input.take() {
                    break input;
                }
                slots = shared.wake.wait(slots).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };

        let started = Instant::now();
        let result = model.infer(&input, &mut matte);
        shared.inferences.fetch_add(1, Ordering::Relaxed);
        shared.inference_micros.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);

        let mut slots = lock();
        slots.spare = Some(input);
        match result {
            Ok(()) => slots.matte = Some(Arc::new(Matte { frame: number, width, height, alpha: matte.clone() })),
            Err(err) => slots.failed = Some(format!("segmentation failed: {err}")),
        }
        drop(slots);
        shared.done.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gray_normalizes_per_model() {
        // Mid-gray YUV (126, 128, 128) is RGB ~127.
        let (w, h) = (64, 36);
        let mut frame = vec![126u8; w * h];
        frame.extend(std::iter::repeat_n(128u8, w * h / 2));

        let mut input = Vec::new();
        nv12_to_input(&frame, w, h, ModelKind::Modnet, &mut input);
        assert_eq!(input.len(), 3 * 512 * 288);
        assert!(input.iter().all(|v| v.abs() < 0.02), "{:?}", &input[..4]);

        nv12_to_input(&frame, w, h, ModelKind::Rvm, &mut input);
        assert_eq!(input.len(), 3 * 960 * 540);
        assert!(input.iter().all(|v| (v - 0.5).abs() < 0.01), "{:?}", &input[..4]);
    }

    #[test]
    fn yuv_extremes() {
        assert_eq!(yuv_to_rgb(235, 128, 128), [255, 255, 255]);
        assert_eq!(yuv_to_rgb(16, 128, 128), [0, 0, 0]);
    }

    /// Runs a real RVM model on synthetic frames when it has been downloaded
    /// (skipped otherwise): checks precision detection, matte size and range,
    /// and that recurrent state is carried between frames.
    fn check_rvm(file: &str, expect_half: bool) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models").join(file);
        if !path.is_file() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let mut model = Model::load(&path, false).unwrap();
        assert_eq!(model.kind, ModelKind::Rvm);
        assert_eq!(model.is_half(), expect_half);
        let (w, h) = ModelKind::Rvm.input_size();
        let input = vec![0.5f32; 3 * w * h];
        let mut matte = Vec::new();
        model.infer(&input, &mut matte).unwrap();
        assert_eq!(matte.len(), w * h);
        assert!(matte.iter().all(|a| (0.0..=1.0).contains(a)));
        let carried = match &model.states {
            RvmStates::F32(states) => states.iter().all(|s| s.as_ref().is_some_and(|(_, data)| data.len() > 1)),
            RvmStates::F16(states) => states.iter().all(|s| s.as_ref().is_some_and(|(_, data)| data.len() > 1)),
        };
        assert!(carried, "recurrent state was not kept");
        model.infer(&input, &mut matte).unwrap();
    }

    #[test]
    fn rvm_fp32_runs_and_carries_state_when_available() {
        check_rvm(RVM_FP32, false);
    }

    #[test]
    fn rvm_fp16_runs_and_carries_state_when_available() {
        check_rvm(RVM_FP16, true);
    }

    #[test]
    fn rvm_preference_depends_on_backend() {
        // Only meaningful when both files are present.
        if let (Some(fp16), Some(fp32)) = (find_model(RVM_FP16), find_model(RVM_FP32)) {
            assert_eq!(rvm_path(true), Some(fp16));
            assert_eq!(rvm_path(false), Some(fp32));
        }
    }
}
