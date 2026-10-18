//! Prints an ONNX model's inputs and outputs: `cargo run --example model_info -- path\to\model.onnx`

use ort::session::Session;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: model_info MODEL.onnx")?;
    let environment = ort::init().with_name("model-info").build().map_err(|e| e.to_string())?;
    let session = Session::builder(&environment).map_err(|e| e.to_string())?.commit_from_file(&path).map_err(|e| e.to_string())?;
    println!("{path}");
    for input in session.inputs() {
        println!("  input  {:<18} {}", input.name(), input.dtype());
    }
    for output in session.outputs() {
        println!("  output {:<18} {}", output.name(), output.dtype());
    }
    Ok(())
}
