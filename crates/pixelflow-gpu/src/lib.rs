//! `PixelFlow` GPU execution: adapter selection, GPU-resident
//! images, and GPU compute nodes.
//!
//! The CPU path stays: it is the fallback, the reference, and
//! the only path for algorithms without a GPU implementation.
//! The GPU path runs when a [`GpuContext`] selects an adapter
//! and a node with a compute implementation exists for the
//! algorithm. A GPU node that cannot run fails loudly with
//! the reason; it never falls back to CPU mid-run.
//!
//! The execution model is explicit at every boundary: upload
//! and download are measured transfers ([`TransferMeasurement`]),
//! compatible GPU stages keep residency through [`GpuImage`],
//! and the compute dispatch waits for the device before any
//! read of its output.

pub mod algorithm;
pub mod backend;
pub mod context;
pub mod error;
pub mod image;
pub mod nearest;

pub use algorithm::{NEAREST_GPU_ID, NearestGpuAlgorithm, nearest_gpu_descriptor};
pub use backend::{ExecutionBackend, GPU_ALGORITHMS, GpuCapability, backend_for, gpu_capability};
pub use context::{AdapterCandidate, AdapterSelection, GpuContext, adapter_rank, select_adapter};
pub use error::GpuError;
pub use image::{
    GpuImage, TEXTURE_BYTES_PER_PIXEL, TransferDirection, TransferMeasurement,
    download_buffer_bytes, padded_row_bytes, upload_bytes,
};
pub use nearest::{GpuNearestNode, GpuRunTimings, NEAREST_SUPPORTED_SCALES, NEAREST_WGSL};
