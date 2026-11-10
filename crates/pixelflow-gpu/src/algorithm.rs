//! The GPU nearest algorithm as a registry entry: the
//! [`AlgorithmFactory`] for the "nearest-gpu" id, so the
//! registry and the live path select the GPU node through the
//! same contract surface as the CPU algorithms.
//!
//! The descriptor is built without a device: it states what
//! the GPU node does, its scales, and that it runs on the
//! GPU backend only, so inspection and listing work on a
//! machine with no adapter. Node construction is the step
//! that needs hardware: [`NearestGpuAlgorithm::create_nodes`]
//! creates the [`GpuContext`] then, and a machine with no
//! adapter fails there loudly with the reason instead of
//! silently substituting the CPU node. The CPU fallback is
//! the caller's decision, made before graph construction
//! from the adapter probe, never inside this factory.

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::nearest::{GpuNearestNode, NEAREST_SUPPORTED_SCALES};
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, AlgorithmOrigin,
    ExecutionBackend, Node, PixelFormat, SpatialDependency, StrideSupport, TemporalDependency,
    validate_config,
};
use std::sync::Arc;

/// The registry id of the GPU nearest algorithm.
pub const NEAREST_GPU_ID: &str = "nearest-gpu";

/// The static description of the GPU nearest algorithm: the
/// GPU node described through the same fields as the CPU
/// algorithms, with the GPU backend declared and the CPU
/// backend absent. The working-memory figure is `None`: the
/// GPU working set is textures, not a flat per-output-pixel
/// CPU figure, and stating the CPU figure would be false.
#[must_use]
pub fn nearest_gpu_descriptor() -> AlgorithmDescriptor {
    AlgorithmDescriptor {
        id: NEAREST_GPU_ID,
        display_name: "Nearest (GPU)",
        description: "nearest-neighbour baseline on the GPU: each source pixel becomes a scale by scale block of identical pixels, computed by a wgpu compute shader",
        version: env!("CARGO_PKG_VERSION"),
        input_formats: vec![PixelFormat::Rgb8],
        output_formats: vec![PixelFormat::Rgb8],
        supported_scales: NEAREST_SUPPORTED_SCALES.to_vec(),
        backends: vec![ExecutionBackend::Gpu],
        origin: AlgorithmOrigin::BuiltIn,
        spatial_dependency: SpatialDependency::Local,
        temporal_dependency: TemporalDependency::Stateless,
        stride_support: StrideSupport::Any,
        tile_safe: true,
        parameters: Vec::new(),
        working_bytes_per_output_pixel: None,
        intermediate_resources: Vec::new(),
        deterministic: true,
        determinism_note: "deterministic: the same input frame, scale, and configuration produce byte-identical output",
        stages: vec![NEAREST_GPU_ID],
    }
}

/// The registry entry for GPU nearest: a built-in, GPU-only,
/// single-stage algorithm with no parameters. It holds no
/// device: the context is created when nodes are built, so
/// registering and inspecting the algorithm never touches
/// hardware.
pub struct NearestGpuAlgorithm;

impl AlgorithmFactory for NearestGpuAlgorithm {
    fn descriptor(&self) -> AlgorithmDescriptor {
        nearest_gpu_descriptor()
    }

    /// There is no CPU processor for this id: the GPU node
    /// is the implementation, and a CPU execution of it
    /// would be a silent backend substitution. Callers that
    /// want CPU execution use the "nearest" id.
    fn create(
        &self,
        _config: &AlgorithmConfig,
    ) -> Result<Box<dyn pixelflow_core::Processor>, AlgorithmError> {
        Err(AlgorithmError::UnsupportedBackend {
            algorithm: NEAREST_GPU_ID,
            backend: ExecutionBackend::Cpu,
        })
    }

    /// Build the GPU nearest node at `scale`: create the GPU
    /// context first, then the node on it. The context
    /// creation is the adapter check: no adapter fails here
    /// with the reason, never with a CPU node.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnsupportedScale`] for a
    /// scale outside [`NEAREST_SUPPORTED_SCALES`],
    /// [`AlgorithmError::InvalidConfig`] for a set parameter
    /// (this algorithm takes none), or
    /// [`AlgorithmError::GraphBuild`] when no adapter exists
    /// or the node cannot be built.
    fn create_nodes(
        &self,
        config: &AlgorithmConfig,
        scale: u32,
    ) -> Result<Vec<Box<dyn Node>>, AlgorithmError> {
        if !NEAREST_SUPPORTED_SCALES.contains(&scale) {
            return Err(AlgorithmError::UnsupportedScale {
                algorithm: NEAREST_GPU_ID,
                scale,
                supported: NEAREST_SUPPORTED_SCALES.to_vec(),
            });
        }
        validate_config(&[], NEAREST_GPU_ID, config)?;
        let context = Arc::new(
            GpuContext::new().map_err(|error| AlgorithmError::GraphBuild(error.to_string()))?,
        );
        let node = GpuNearestNode::new(context, scale).map_err(|error| match error {
            GpuError::UnsupportedScale { scale } => AlgorithmError::UnsupportedScale {
                algorithm: NEAREST_GPU_ID,
                scale,
                supported: NEAREST_SUPPORTED_SCALES.to_vec(),
            },
            error => AlgorithmError::GraphBuild(error.to_string()),
        })?;
        Ok(vec![Box::new(node)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_declares_the_gpu_backend_only() {
        let descriptor = nearest_gpu_descriptor();
        assert_eq!(descriptor.id, "nearest-gpu");
        assert_eq!(descriptor.display_name, "Nearest (GPU)");
        assert_eq!(descriptor.backends, vec![ExecutionBackend::Gpu]);
        assert_eq!(descriptor.supported_scales, vec![2, 4]);
        assert_eq!(descriptor.stages, vec!["nearest-gpu"]);
        assert_eq!(descriptor.input_formats, vec![PixelFormat::Rgb8]);
        assert_eq!(descriptor.output_formats, vec![PixelFormat::Rgb8]);
        assert_eq!(descriptor.spatial_dependency, SpatialDependency::Local);
        assert_eq!(
            descriptor.temporal_dependency,
            TemporalDependency::Stateless
        );
        assert!(descriptor.tile_safe);
        assert!(descriptor.deterministic);
        assert!(descriptor.parameters.is_empty());
        assert_eq!(descriptor.working_bytes_per_output_pixel, None);
        assert_eq!(descriptor.origin, AlgorithmOrigin::BuiltIn);
    }

    #[test]
    fn factory_descriptor_matches_the_free_function() {
        assert_eq!(NearestGpuAlgorithm.descriptor(), nearest_gpu_descriptor());
    }

    #[test]
    fn create_has_no_cpu_processor() {
        let result = NearestGpuAlgorithm.create(&AlgorithmConfig::new());
        assert_eq!(
            result.err(),
            Some(AlgorithmError::UnsupportedBackend {
                algorithm: "nearest-gpu",
                backend: ExecutionBackend::Cpu,
            })
        );
    }

    #[test]
    fn create_nodes_rejects_an_unsupported_scale_without_a_device() {
        let result = NearestGpuAlgorithm.create_nodes(&AlgorithmConfig::new(), 3);
        assert_eq!(
            result.err(),
            Some(AlgorithmError::UnsupportedScale {
                algorithm: "nearest-gpu",
                scale: 3,
                supported: vec![2, 4],
            })
        );
    }

    #[test]
    fn create_nodes_rejects_a_set_parameter_without_a_device() {
        let mut config = AlgorithmConfig::new();
        config.set(
            "memory_budget_bytes",
            pixelflow_core::ConfigValue::Integer(1024),
        );
        let result = NearestGpuAlgorithm.create_nodes(&config, 2);
        assert!(
            matches!(result, Err(AlgorithmError::InvalidConfig { .. })),
            "a set parameter fails validation before any device is touched"
        );
    }

    #[test]
    fn create_nodes_without_an_adapter_fails_loudly_or_builds_one_node() {
        // The probe and the constructor enumerate the same
        // adapters, so they agree: no adapter fails here
        // with the reason, and an adapter builds exactly
        // one node named "nearest-gpu".
        if GpuContext::probe_adapters().is_empty() {
            match NearestGpuAlgorithm.create_nodes(&AlgorithmConfig::new(), 2) {
                Err(AlgorithmError::GraphBuild(message)) => {
                    assert!(
                        message.contains("no GPU adapter"),
                        "no adapter fails loudly with the reason, got {message}"
                    );
                }
                Err(error) => panic!("expected a no-adapter graph build error, got {error}"),
                Ok(_) => panic!("expected failure without an adapter"),
            }
        } else {
            let nodes = NearestGpuAlgorithm
                .create_nodes(&AlgorithmConfig::new(), 2)
                .map_err(|error| error.to_string())
                .expect("an adapter builds the GPU node");
            assert_eq!(nodes.len(), 1);
            assert_eq!(nodes[0].name(), "nearest-gpu");
        }
    }
}
