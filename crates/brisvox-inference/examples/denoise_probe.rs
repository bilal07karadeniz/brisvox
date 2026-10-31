//! Does a streaming denoiser ONNX file load, and what does it expect? (dev-only harness.)
//!
//! Prints the inputs and outputs (names, element types, shapes) the file declares, as seen by
//! ONNX Runtime (`ort`) and by tract, then tries to run one streaming step with zero tensors in
//! each runtime. Used to confirm the facts `docs/providers/noise-suppression.md` records about
//! FastEnhancer's waveform graphs.
//!
//! Run: `cargo run --release -p brisvox-inference --features smart-turn-ort --example
//! denoise_probe -- <model.onnx>`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::indexing_slicing
)] // dev harness, not library code

use tract_onnx::prelude::*;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: denoise_probe <model.onnx>");
    println!("== ort ==");
    let mut session = ort::session::Session::builder()
        .unwrap()
        .with_intra_threads(1)
        .unwrap()
        .commit_from_file(&path)
        .unwrap();
    for input in session.inputs() {
        println!("input  {:?}: {:?}", input.name(), input.dtype());
    }
    for output in session.outputs() {
        println!("output {:?}: {:?}", output.name(), output.dtype());
    }
    // One step with zeros, shapes taken from the declared input types (dynamic dims -> 1).
    let mut inputs: Vec<(
        std::borrow::Cow<'static, str>,
        ort::session::SessionInputValue<'static>,
    )> = Vec::new();
    for input in session.inputs() {
        let shape: Vec<i64> = input
            .dtype()
            .tensor_shape()
            .map(|s| s.iter().map(|d| if *d < 1 { 1 } else { *d }).collect())
            .unwrap_or_default();
        let len: i64 = shape.iter().product();
        let data = vec![0.0_f32; len as usize];
        let tensor = ort::value::Tensor::from_array((shape.clone(), data)).unwrap();
        println!("  feeding {:?} shape {shape:?}", input.name());
        inputs.push((
            input.name().to_owned().into(),
            ort::session::SessionInputValue::from(tensor.into_dyn()),
        ));
    }
    let outputs = session.run(inputs).unwrap();
    for (name, value) in outputs.iter() {
        println!("  ran: output {name:?} {:?}", value.dtype());
    }

    println!("== tract ==");
    match tract_onnx::onnx().model_for_path(&path) {
        Err(e) => println!("tract: cannot parse: {e:#}"),
        Ok(model) => {
            for (i, outlet) in model.input_outlets().unwrap().iter().enumerate() {
                println!(
                    "input  {i} {:?}: {:?}",
                    model.node(outlet.node).name,
                    model.outlet_fact(*outlet).unwrap()
                );
            }
            for (i, outlet) in model.output_outlets().unwrap().iter().enumerate() {
                println!(
                    "output {i} {:?}: {:?}",
                    model.node(outlet.node).name,
                    model.outlet_fact(*outlet).unwrap()
                );
            }
            match model.into_optimized() {
                Err(e) => println!("tract: cannot optimize: {e:#}"),
                Ok(m) => match m.into_runnable() {
                    Err(e) => println!("tract: cannot make runnable: {e:#}"),
                    Ok(_) => println!("tract: loaded and optimized"),
                },
            }
        }
    }
}
