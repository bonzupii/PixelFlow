//! Memory budget enforcement for Contour.
//!
//! The budget is 2 GiB at 128 bytes per output pixel, so
//! 16,777,216 output pixels is exactly the budget: 4096x4096
//! passes, anything larger fails. The numbers are derived from the
//! measured peak-RSS slope in `docs/performance.md`, not guessed.

use pixelflow_core::{Frame, PixelFormat, ProcessError, Processor};
use pixelflow_processing::Contour;

#[test]
fn budget_boundary_at_exactly_the_budget_passes() -> Result<(), String> {
    // 4096 * 4096 = 16,777,216 pixels * 128 bytes = 2,147,483,648
    // bytes, exactly the budget. The check refuses only what
    // exceeds it.
    Contour::check_memory_budget(4096, 4096).map_err(|e| e.to_string())
}

#[test]
fn budget_boundary_just_under_the_budget_passes() -> Result<(), String> {
    // 4096 * 4095 = 16,773,120 pixels * 128 bytes = 2,146,959,360
    // bytes, 524,288 bytes under the budget.
    Contour::check_memory_budget(4096, 4095).map_err(|e| e.to_string())
}

#[test]
fn budget_boundary_one_row_over_fails_with_the_typed_error() -> Result<(), String> {
    // 4097 * 4096 = 16,781,312 pixels * 128 bytes = 2,148,007,936
    // bytes, 524,288 bytes over the budget.
    match Contour::check_memory_budget(4097, 4096) {
        Err(ProcessError::MemoryBudgetExceeded {
            output_pixels,
            estimated_bytes,
            budget_bytes,
            ..
        }) => {
            assert_eq!(output_pixels, 16_781_312);
            assert_eq!(estimated_bytes, 2_148_007_936);
            assert_eq!(budget_bytes, 2_147_483_648);
            Ok(())
        }
        other => Err(format!("expected MemoryBudgetExceeded, got {other:?}")),
    }
}

#[test]
fn oversized_input_is_refused_with_the_typed_error() -> Result<(), String> {
    // 2048x2048 at 4x is 8192x8192 = 67,108,864 output pixels,
    // an 8 GiB estimate against a 2 GiB budget. The refusal
    // happens before any of that memory is allocated, so this
    // test builds only the 12 MiB input frame.
    let frame = Frame::new(2048, 2048, PixelFormat::Rgb8, vec![128; 2048 * 2048 * 3])
        .map_err(|e| e.to_string())?;
    match Contour.process(&frame, 4) {
        Err(ProcessError::MemoryBudgetExceeded {
            output_pixels,
            estimated_bytes,
            budget_bytes,
            ..
        }) => {
            assert_eq!(output_pixels, 67_108_864);
            assert_eq!(estimated_bytes, 8_589_934_592);
            assert_eq!(budget_bytes, 2_147_483_648);
            Ok(())
        }
        other => Err(format!("expected MemoryBudgetExceeded, got {other:?}")),
    }
}
