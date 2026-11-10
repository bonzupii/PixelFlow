//! GPU probe: list the adapters on this machine, and when
//! one exists run the GPU nearest node against the CPU
//! nearest algorithm and print the parity result and the
//! measured timings. Exit status is nonzero when an adapter
//! exists and the GPU run fails or disagrees with the CPU
//! reference; with no adapter it reports that and exits zero,
//! because absence of hardware is not a failure.

use pixelflow_core::{Frame, PixelFormat, Processor};
use pixelflow_gpu::{GpuContext, GpuNearestNode};
use pixelflow_processing::Nearest;
use std::sync::Arc;

fn main() -> Result<(), String> {
    let adapters = GpuContext::probe_adapters();
    if adapters.is_empty() {
        println!("no GPU adapter found on this machine");
        return Ok(());
    }
    for adapter in &adapters {
        println!(
            "adapter: {} ({:?} on {:?}, driver {})",
            adapter.name(),
            adapter.device_type(),
            adapter.backend(),
            adapter.driver()
        );
    }
    let context = Arc::new(GpuContext::new().map_err(|error| error.to_string())?);
    println!("selected: {}", context.selection());
    let mut data = Vec::with_capacity(4 * 3 * 3);
    for y in 0..3u8 {
        for x in 0..4u8 {
            data.extend_from_slice(&[x.wrapping_mul(40), y.wrapping_mul(70), x ^ y]);
        }
    }
    let input = Frame::new(4, 3, PixelFormat::Rgb8, data).map_err(|error| error.to_string())?;
    for scale in [2, 4] {
        let expected = Nearest
            .process(&input, scale)
            .map_err(|error| error.to_string())?;
        let node =
            GpuNearestNode::new(Arc::clone(&context), scale).map_err(|error| error.to_string())?;
        let (actual, timings) = node.run_gpu(&input).map_err(|error| error.to_string())?;
        if actual.data() != expected.data() {
            return Err(format!(
                "GPU nearest output differs from CPU nearest at scale {scale}"
            ));
        }
        println!(
            "scale {scale}: parity exact, upload {:?}, dispatch {:?}, download {:?}, total {:?}",
            timings.upload(),
            timings.dispatch(),
            timings.download(),
            timings.total()
        );
    }
    Ok(())
}
