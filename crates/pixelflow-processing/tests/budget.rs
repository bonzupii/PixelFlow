//! Configurable memory budget for Contour.
//!
//! `Contour` on its own sets no budget: any size the machine can
//! allocate is attempted. `Contour::with_memory_budget` returns a
//! processor that refuses a frame whose estimated working set
//! exceeds the budget, with the typed error. The estimate is the
//! working-set model in `Contour::check_memory_budget` (output
//! image, follower map, final conversion, input, and one strip's
//! working set per worker thread), in checked arithmetic. The
//! numbers below are derived from that model, not guessed: strip
//! rows come from the 256 MiB strip target at 96 bytes per output
//! pixel, and the strip term is counted once per worker thread
//! (the direct-check tests pass 4 threads explicitly, so the
//! numbers are exact on any machine).

use pixelflow_core::{Frame, PixelFormat, ProcessError, Processor};
use pixelflow_processing::Contour;

#[test]
fn no_budget_passes_a_size_the_old_ceiling_refused() -> Result<(), String> {
    // 4097 * 4096 = 16,781,312 output pixels at 4x. The removed
    // fixed ceiling refused anything over 16,777,216. With no
    // budget configured the check passes: the size arithmetic
    // still has to fit, and it does.
    Contour::check_memory_budget(None, 4, 4097, 4096, 4).map_err(|e| e.to_string())
}

#[test]
fn configured_budget_refuses_with_the_typed_error() -> Result<(), String> {
    // 4096 * 4096 = 16,777,216 output pixels at 4x against a
    // 1,000,000 byte budget. Estimate: 16,777,216 * 19 (output
    // image, follower map, final conversion) + 1,048,576 * 3
    // (input) + 4 * 4096 * 72 * 4 * 96 (four live strips, one
    // per worker thread: 64 rows plus the 4-row halo on each
    // side) = 774,897,664.
    match Contour::check_memory_budget(Some(1_000_000), 4, 4096, 4096, 4) {
        Err(ProcessError::MemoryBudgetExceeded {
            stage,
            output_pixels,
            estimated_bytes,
            budget_bytes,
            ..
        }) => {
            assert_eq!(stage, "budget");
            assert_eq!(output_pixels, 16_777_216);
            assert_eq!(estimated_bytes, 774_897_664);
            assert_eq!(budget_bytes, 1_000_000);
            Ok(())
        }
        other => Err(format!("expected MemoryBudgetExceeded, got {other:?}")),
    }
}

#[test]
fn configured_budget_passes_exactly_at_the_estimate() -> Result<(), String> {
    // The check refuses only what exceeds the budget.
    Contour::check_memory_budget(Some(774_897_664), 4, 4096, 4096, 4).map_err(|e| e.to_string())
}

#[test]
fn oversized_input_is_refused_by_a_configured_budget() -> Result<(), String> {
    // A 256x1 frame at 4x is 1024x4 = 4,096 output pixels
    // against a 1,000,000 byte budget. Estimate: 4,096 * 19 +
    // 256 * 3 + 1024 * 72 * 4 * 96 = 28,390,144. The frame is
    // one row high so exactly one strip is live whatever the
    // machine's thread count is, which keeps this number exact
    // on any machine. The refusal happens before any of that
    // memory is allocated.
    let frame =
        Frame::new(256, 1, PixelFormat::Rgb8, vec![128; 256 * 1 * 3]).map_err(|e| e.to_string())?;
    let limited = Contour::with_memory_budget(1_000_000);
    match limited.process(&frame, 4) {
        Err(ProcessError::MemoryBudgetExceeded {
            stage,
            output_pixels,
            estimated_bytes,
            budget_bytes,
            ..
        }) => {
            assert_eq!(stage, "budget");
            assert_eq!(output_pixels, 4_096);
            assert_eq!(estimated_bytes, 28_390_144);
            assert_eq!(budget_bytes, 1_000_000);
            Ok(())
        }
        other => Err(format!("expected MemoryBudgetExceeded, got {other:?}")),
    }
}

#[test]
fn generous_budget_processes_a_small_frame() -> Result<(), String> {
    // A 1 GiB budget against a 64x48 frame at 2x (12,288 output
    // pixels, about 7 MiB estimated on 4 threads) processes
    // normally through the budgeted processor.
    let frame =
        Frame::new(64, 48, PixelFormat::Rgb8, vec![200; 64 * 48 * 3]).map_err(|e| e.to_string())?;
    let limited = Contour::with_memory_budget(1_073_741_824);
    let out = limited.process(&frame, 2).map_err(|e| e.to_string())?;
    assert_eq!((out.width(), out.height()), (128, 96));
    Ok(())
}

#[test]
fn size_overflow_is_a_typed_error_not_a_panic() -> Result<(), String> {
    // u32::MAX by u32::MAX at 4x: the output image alone is
    // about 2.2e20 bytes, past u64. The check fails with the
    // typed overflow error, with or without a budget, and never
    // wraps or panics.
    match Contour::check_memory_budget(None, 4, u32::MAX, u32::MAX, 4) {
        Err(ProcessError::SizeOverflow { stage, .. }) => {
            assert_eq!(stage, "budget");
            Ok(())
        }
        other => Err(format!("expected SizeOverflow, got {other:?}")),
    }?;
    match Contour::check_memory_budget(Some(1_000_000), 2, u32::MAX, u32::MAX, 4) {
        Err(ProcessError::SizeOverflow { stage, .. }) => {
            assert_eq!(stage, "budget");
            Ok(())
        }
        other => Err(format!("expected SizeOverflow, got {other:?}")),
    }
}
