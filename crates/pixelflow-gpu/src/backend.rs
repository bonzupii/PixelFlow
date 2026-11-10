//! Execution backend and per-algorithm GPU capability: the
//! GPU-side declarations the framework wires into the core
//! contract at integration.

use std::fmt;

/// Where a node runs: the CPU path that already exists, or the
/// GPU compute path this crate provides. CPU stays available
/// as fallback, reference, and for algorithms without GPU
/// implementations; it is never the default when a GPU adapter
/// exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionBackend {
    /// Synchronous CPU execution, the existing path.
    Cpu,
    /// wgpu compute execution on the selected adapter.
    Gpu,
}

impl ExecutionBackend {
    /// Short stable name, for configuration and diagnostics.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ExecutionBackend::Cpu => "cpu",
            ExecutionBackend::Gpu => "gpu",
        }
    }
}

impl fmt::Display for ExecutionBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Whether an algorithm has a GPU implementation in this
/// crate, and what kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuCapability {
    /// No GPU implementation exists; the algorithm runs on
    /// CPU only.
    NotImplemented,
    /// A wgpu compute implementation exists; the algorithm
    /// runs as a GPU compute node when an adapter is present.
    Compute,
}

/// Algorithm names with a GPU compute implementation in this
/// crate, in the order they were added.
pub const GPU_ALGORITHMS: &[&str] = &["nearest"];

/// The GPU capability of the algorithm named `algorithm`, by
/// its stable processor name (for example `"nearest"`).
/// Anything not listed here has no GPU implementation.
#[must_use]
pub fn gpu_capability(algorithm: &str) -> GpuCapability {
    if GPU_ALGORITHMS.contains(&algorithm) {
        GpuCapability::Compute
    } else {
        GpuCapability::NotImplemented
    }
}

/// The backend an algorithm runs on when `adapter_present`
/// says whether a GPU adapter was selected: GPU for an
/// algorithm with a compute implementation on a machine with
/// an adapter, CPU otherwise. CPU is the fallback, never a
/// silent substitution inside a GPU node.
#[must_use]
pub fn backend_for(algorithm: &str, adapter_present: bool) -> ExecutionBackend {
    if adapter_present && gpu_capability(algorithm) == GpuCapability::Compute {
        ExecutionBackend::Gpu
    } else {
        ExecutionBackend::Cpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_has_compute_and_unknown_algorithms_do_not() {
        assert_eq!(gpu_capability("nearest"), GpuCapability::Compute);
        assert_eq!(gpu_capability("contour"), GpuCapability::NotImplemented);
        assert_eq!(gpu_capability(""), GpuCapability::NotImplemented);
    }

    #[test]
    fn backend_is_gpu_only_with_adapter_and_capability() {
        assert_eq!(backend_for("nearest", true), ExecutionBackend::Gpu);
        assert_eq!(backend_for("nearest", false), ExecutionBackend::Cpu);
        assert_eq!(backend_for("contour", true), ExecutionBackend::Cpu);
        assert_eq!(backend_for("contour", false), ExecutionBackend::Cpu);
    }

    #[test]
    fn backend_names_are_stable() {
        assert_eq!(ExecutionBackend::Cpu.name(), "cpu");
        assert_eq!(ExecutionBackend::Gpu.name(), "gpu");
        assert_eq!(ExecutionBackend::Gpu.to_string(), "gpu");
    }
}
