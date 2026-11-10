//! The GPU nearest factory in the registry: lookup and
//! descriptor through the same surface as the CPU algorithms.
//! Registration and inspection need no device, so these
//! checks run on a machine with no adapter; only node
//! construction touches hardware, and that is covered by the
//! factory's own tests and the parity test.

use pixelflow_core::{AlgorithmError, ExecutionBackend};
use pixelflow_gpu::NearestGpuAlgorithm;
use pixelflow_processing::AlgorithmRegistry;

#[test]
fn builtin_registry_holds_only_the_cpu_algorithms() {
    let registry = AlgorithmRegistry::builtin();
    assert_eq!(
        registry.ids(),
        vec![
            "contour",
            "nearest",
            "bilinear",
            "bicubic",
            "lanczos3",
            "scale2x",
            "pixelflow"
        ]
    );
    assert!(matches!(
        registry.descriptor("nearest-gpu"),
        Err(AlgorithmError::UnknownAlgorithm { .. })
    ));
}

#[test]
fn gpu_nearest_registers_and_describes_through_the_registry() {
    let mut registry = AlgorithmRegistry::builtin();
    registry.register(Box::new(NearestGpuAlgorithm));
    assert_eq!(
        registry.ids(),
        vec![
            "contour",
            "nearest",
            "bilinear",
            "bicubic",
            "lanczos3",
            "scale2x",
            "pixelflow",
            "nearest-gpu"
        ]
    );
    let descriptor = registry
        .descriptor("nearest-gpu")
        .map_err(|error| error.to_string())
        .expect("the registered GPU algorithm describes itself");
    assert_eq!(descriptor.id, "nearest-gpu");
    assert_eq!(descriptor.display_name, "Nearest (GPU)");
    assert_eq!(descriptor.backends, vec![ExecutionBackend::Gpu]);
    assert_eq!(descriptor.supported_scales, vec![2, 4]);
    assert_eq!(descriptor.stages, vec!["nearest-gpu"]);
    assert!(descriptor.tile_safe);
    assert!(descriptor.deterministic);
}

#[test]
fn gpu_nearest_lookup_fails_with_the_available_ids() {
    let registry = AlgorithmRegistry::builtin();
    match registry.descriptor("nearest-gpu") {
        Err(AlgorithmError::UnknownAlgorithm { id, available }) => {
            assert_eq!(id, "nearest-gpu");
            assert_eq!(
                available,
                vec![
                    "contour",
                    "nearest",
                    "bilinear",
                    "bicubic",
                    "lanczos3",
                    "scale2x",
                    "pixelflow"
                ]
            );
        }
        other => panic!("expected an unknown-algorithm error, got {other:?}"),
    }
}
