//! Runs RVM on the same moving frame sequence with the CPU and DirectML
//! execution providers and compares the mattes, to catch backend-specific
//! numerical problems.
//!
//! `cargo run --release --example rvm_backends -- models\rvm_mobilenetv3_fp32.onnx IMAGE.jpg`

use ort::ep;
use ort::session::Session;
use ort::value::{Shape, TensorRef};

const W: usize = 960;
const H: usize = 540;

type States = [Option<(Shape, Vec<f32>)>; 4];

fn run(session: &mut Session, states: &mut States, src: &[f32], ratio: f32) -> Result<Vec<f32>, String> {
    let zero = [0.0f32];
    let ratio = [ratio];
    let state = |i: usize| match &states[i] {
        Some((shape, data)) => TensorRef::from_array_view((shape.clone(), data.as_slice())),
        None => TensorRef::from_array_view(([1usize, 1, 1, 1], &zero[..])),
    };
    let e = |e: ort::Error| e.to_string();
    let inputs = ort::inputs![
        "src" => TensorRef::from_array_view(([1usize, 3, H, W], src)).map_err(e)?,
        "r1i" => state(0).map_err(e)?,
        "r2i" => state(1).map_err(e)?,
        "r3i" => state(2).map_err(e)?,
        "r4i" => state(3).map_err(e)?,
        "downsample_ratio" => TensorRef::from_array_view(([1usize], &ratio[..])).map_err(e)?,
    ];
    let outputs = session.run(inputs).map_err(|e| e.to_string())?;
    let alpha = outputs["pha"].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
    let mut next: States = Default::default();
    for (i, slot) in next.iter_mut().enumerate() {
        let (shape, data) = outputs[format!("r{}o", i + 1).as_str()].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        *slot = Some((shape.clone(), data.to_vec()));
    }
    drop(outputs);
    *states = next;
    Ok(alpha)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let model = args.next().ok_or("usage: rvm_backends MODEL IMAGE [RATIO]")?;
    let image = args.next().ok_or("usage: rvm_backends MODEL IMAGE [RATIO]")?;
    let ratio: f32 = args.next().and_then(|r| r.parse().ok()).unwrap_or(0.5);
    let picture = image::open(&image)?.resize_exact(1440, 810, image::imageops::FilterType::Triangle).to_rgb8();

    let env = ort::init().with_name("rvm-backends").build().map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&model)?;
    let mut cpu = Session::builder(&env).map_err(|e| e.to_string())?.commit_from_memory(&bytes).map_err(|e| e.to_string())?;
    let mut gpu = Session::builder(&env)
        .map_err(|e| e.to_string())?
        .with_memory_pattern(false)
        .map_err(|e| e.to_string())?
        .with_parallel_execution(false)
        .map_err(|e| e.to_string())?
        .with_execution_providers([ep::DirectML::default().build().error_on_failure()])
        .map_err(|e| e.to_string())?
        .commit_from_memory(&bytes)
        .map_err(|e| e.to_string())?;

    let (mut cpu_states, mut gpu_states): (States, States) = Default::default();
    let mut src = vec![0.0f32; 3 * W * H];
    println!("ratio {ratio}");
    println!("frame  cpu_mean  gpu_mean  mean|diff|  max|diff|  gpu_ms");
    for frame in 0..40usize {
        // Pan across the picture (8 px per frame, back and forth) to simulate movement.
        let offset = (frame * 8) % 960;
        let x0 = if offset <= 480 { offset } else { 960 - offset };
        for y in 0..H {
            for x in 0..W {
                let px = picture.get_pixel((x0 + x) as u32, (135 + y) as u32).0;
                for c in 0..3 {
                    src[c * W * H + y * W + x] = px[c] as f32 / 255.0;
                }
            }
        }
        let a = run(&mut cpu, &mut cpu_states, &src, ratio)?;
        let started = std::time::Instant::now();
        let b = run(&mut gpu, &mut gpu_states, &src, ratio)?;
        let gpu_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        let diffs: Vec<f32> = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).collect();
        println!(
            "{frame:>5}  {:>8.4}  {:>8.4}  {:>10.4}  {:>9.4}  {gpu_ms:>6.1}",
            mean(&a),
            mean(&b),
            mean(&diffs),
            diffs.iter().cloned().fold(0.0, f32::max)
        );
    }
    Ok(())
}
