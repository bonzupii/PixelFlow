//! GPU nearest parity through the graph executor: the GPU
//! node's output must be pixel-identical to the CPU nearest
//! algorithm on the same inputs. The test runs only when an
//! adapter exists; on a machine with no adapter it reports
//! the skip and passes, because absence of hardware is not a
//! failure of the node. A device run is the only thing that
//! proves GPU execution: the naga shader validation in the
//! unit tests proves the shader parses, not that it runs.

use pixelflow_core::graph::{GraphBuilder, Source};
use pixelflow_core::{Frame, PixelFormat, Processor};
use pixelflow_gpu::{GpuContext, GpuNearestNode};
use pixelflow_processing::Nearest;
use std::sync::Arc;

/// A 4 by 3 frame with distinct pixels, so a wrong scale or a
/// shifted copy shows up in the comparison.
fn test_frame() -> Result<Frame, String> {
    let mut data = Vec::with_capacity(4 * 3 * 3);
    for y in 0..3u8 {
        for x in 0..4u8 {
            data.extend_from_slice(&[x.wrapping_mul(40), y.wrapping_mul(70), x ^ y]);
        }
    }
    Frame::new(4, 3, PixelFormat::Rgb8, data)
        .map_err(|error| error.to_string())
        .map(|frame| frame.with_sequence(7))
}

/// Run one scale through a one-node graph holding the GPU
/// nearest node, and compare with the CPU nearest output.
fn check_scale(context: &Arc<GpuContext>, scale: u32) -> Result<(), String> {
    let input = test_frame()?;
    let expected = Nearest
        .process(&input, scale)
        .map_err(|error| error.to_string())?;
    let node =
        GpuNearestNode::new(Arc::clone(context), scale).map_err(|error| error.to_string())?;
    let mut builder = GraphBuilder::new();
    // `build` takes the builder by value, so the chained
    // `&mut Self` is moved out with `take`, not discarded.
    let mut graph = std::mem::take(
        builder
            .add_input("in", PixelFormat::Rgb8)
            .add_node("nearest-gpu", Box::new(node))
            .connect(Source::input("in"), "nearest-gpu", "in")
            .add_output("out", "nearest-gpu", "out"),
    )
    .build()
    .map_err(|error| error.to_string())?;
    let outputs = graph
        .run(vec![("in", input)])
        .map_err(|error| error.to_string())?;
    assert_eq!(outputs.len(), 1);
    let (_, actual) = &outputs[0];
    assert_eq!(actual.width(), expected.width());
    assert_eq!(actual.height(), expected.height());
    assert_eq!(actual.data(), expected.data());
    assert_eq!(actual.sequence(), 7);
    Ok(())
}

#[test]
fn gpu_nearest_matches_cpu_nearest_through_the_graph() -> Result<(), String> {
    let context = match GpuContext::new() {
        Ok(context) => Arc::new(context),
        Err(pixelflow_gpu::GpuError::NoAdapter) => {
            eprintln!("skipping GPU parity check: no adapter on this machine");
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };
    eprintln!("adapter: {}", context.selection());
    for scale in [2, 4] {
        check_scale(&context, scale)?;
    }
    let empty =
        Frame::new(0, 0, PixelFormat::Rgb8, Vec::new()).map_err(|error| error.to_string())?;
    let node = GpuNearestNode::new(Arc::clone(&context), 2).map_err(|error| error.to_string())?;
    let (output, _) = node.run_gpu(&empty).map_err(|error| error.to_string())?;
    assert_eq!((output.width(), output.height()), (0, 0));
    Ok(())
}
