//! GPU context: adapter selection with an explicit fallback
//! chain, and the device and queue a GPU node runs on.
//!
//! Selection enumerates every adapter the instance reports,
//! ranks them discrete GPU first, then integrated graphics,
//! then virtual GPU, then CPU last resort, and records which
//! adapter won and why. The ranking is a pure function over
//! adapter descriptors, so it is unit-tested without a device.

use crate::error::GpuError;

/// One enumerated adapter, reduced to the fields selection
/// reads. Built from [`wgpu::AdapterInfo`]; the selection
/// logic below only ever sees this struct, never a live
/// adapter, so it runs anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdapterCandidate {
    /// Adapter name, as the backend reports it.
    name: String,
    /// Backend the adapter came from (Vulkan, GL, and so on).
    backend: wgpu::Backend,
    /// Kind of device: discrete GPU, integrated graphics,
    /// virtual GPU, CPU fallback, or other.
    device_type: wgpu::DeviceType,
    /// Backend-specific vendor ID.
    vendor: u32,
    /// Backend-specific device ID.
    device: u32,
    /// Driver name, as the backend reports it.
    driver: String,
}

impl AdapterCandidate {
    /// Build a candidate from a wgpu adapter's info.
    #[must_use]
    pub fn from_info(info: &wgpu::AdapterInfo) -> Self {
        Self {
            name: info.name.clone(),
            backend: info.backend,
            device_type: info.device_type,
            vendor: info.vendor,
            device: info.device,
            driver: info.driver.clone(),
        }
    }

    /// Adapter name, as the backend reports it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Backend the adapter came from.
    #[must_use]
    pub fn backend(&self) -> wgpu::Backend {
        self.backend
    }

    /// Kind of device.
    #[must_use]
    pub fn device_type(&self) -> wgpu::DeviceType {
        self.device_type
    }

    /// Backend-specific vendor ID.
    #[must_use]
    pub fn vendor(&self) -> u32 {
        self.vendor
    }

    /// Backend-specific device ID.
    #[must_use]
    pub fn device(&self) -> u32 {
        self.device
    }

    /// Driver name, as the backend reports it.
    #[must_use]
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// Selection rank: lower wins. Discrete GPU ranks 0,
    /// integrated graphics 1, virtual GPU 2, CPU fallback 3,
    /// anything else 4. The CPU fallback is last resort by
    /// construction, never preferred over real hardware.
    #[must_use]
    pub fn rank(&self) -> u8 {
        adapter_rank(self.device_type)
    }
}

/// Selection rank of a wgpu device type: lower wins. Discrete
/// GPU ranks 0, integrated graphics 1, virtual GPU 2, CPU
/// fallback 3, anything else 4.
#[must_use]
pub fn adapter_rank(device_type: wgpu::DeviceType) -> u8 {
    match device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Cpu => 3,
        wgpu::DeviceType::Other => 4,
    }
}

/// Pick the winning candidate: the lowest rank, ties broken
/// by enumeration order, so selection is stable across runs
/// on the same machine. Returns the winner's index and the
/// reason string, or `None` when the list is empty.
#[must_use]
pub fn select_adapter(candidates: &[AdapterCandidate]) -> Option<(usize, String)> {
    let mut best: Option<(usize, u8)> = None;
    for (index, candidate) in candidates.iter().enumerate() {
        let rank = candidate.rank();
        let better = match best {
            None => true,
            Some((_, best_rank)) => rank < best_rank,
        };
        if better {
            best = Some((index, rank));
        }
    }
    best.map(|(index, rank)| {
        let winner = &candidates[index];
        let others = candidates.len().saturating_sub(1);
        let reason = format!(
            "rank {rank} ({:?}) over {others} other adapter(s): discrete GPU preferred, then integrated graphics, then CPU last resort",
            winner.device_type
        );
        (index, reason)
    })
}

/// The adapter a [`GpuContext`] runs on, and why it was
/// chosen: its identity plus the selection reason and how
/// many adapters were enumerated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterSelection {
    /// The winning adapter.
    candidate: AdapterCandidate,
    /// Why this adapter won, from [`select_adapter`].
    reason: String,
    /// How many adapters were enumerated in total.
    candidates_seen: usize,
}

impl AdapterSelection {
    /// Adapter name, as the backend reports it.
    #[must_use]
    pub fn name(&self) -> &str {
        self.candidate.name()
    }

    /// Backend the adapter came from.
    #[must_use]
    pub fn backend(&self) -> wgpu::Backend {
        self.candidate.backend()
    }

    /// Kind of device.
    #[must_use]
    pub fn device_type(&self) -> wgpu::DeviceType {
        self.candidate.device_type()
    }

    /// Backend-specific vendor ID.
    #[must_use]
    pub fn vendor(&self) -> u32 {
        self.candidate.vendor()
    }

    /// Backend-specific device ID.
    #[must_use]
    pub fn device(&self) -> u32 {
        self.candidate.device()
    }

    /// Driver name, as the backend reports it.
    #[must_use]
    pub fn driver(&self) -> &str {
        self.candidate.driver()
    }

    /// Why this adapter won selection.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// How many adapters were enumerated in total.
    #[must_use]
    pub fn candidates_seen(&self) -> usize {
        self.candidates_seen
    }
}

impl std::fmt::Display for AdapterSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "adapter '{}' ({:?} on {:?}, driver '{}'): {}",
            self.name(),
            self.device_type(),
            self.backend(),
            self.driver(),
            self.reason()
        )
    }
}

/// The GPU context: the selected adapter's device and queue,
/// plus the record of which adapter was selected and why. One
/// context is shared by every GPU node in a graph, so a frame
/// stays resident between compatible GPU stages instead of
/// crossing the CPU boundary per stage.
pub struct GpuContext {
    /// Logical device on the selected adapter.
    device: wgpu::Device,
    /// Queue that executes command buffers on the device.
    queue: wgpu::Queue,
    /// Which adapter was selected and why.
    selection: AdapterSelection,
}

impl GpuContext {
    /// Enumerate the adapters on this machine without creating
    /// a device: the diagnostic view of what selection has to
    /// choose from. An empty list means no backend answered at
    /// all, which is how a machine with no GPU and no software
    /// adapter reports itself.
    #[must_use]
    pub fn probe_adapters() -> Vec<AdapterCandidate> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()))
            .iter()
            .map(|adapter| AdapterCandidate::from_info(&adapter.get_info()))
            .collect()
    }

    /// Create a context on the best available adapter: the
    /// discrete GPU when one exists, integrated graphics
    /// otherwise, the CPU fallback adapter last. The device
    /// requests default features and limits: compute-only
    /// work needs nothing beyond the defaults.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::NoAdapter`] when no adapter was
    /// enumerated, and [`GpuError::DeviceRequest`] when the
    /// selected adapter refused to create a device.
    pub fn new() -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        let candidates: Vec<AdapterCandidate> = adapters
            .iter()
            .map(|adapter| AdapterCandidate::from_info(&adapter.get_info()))
            .collect();
        let (index, reason) = select_adapter(&candidates).ok_or(GpuError::NoAdapter)?;
        let selection = AdapterSelection {
            candidate: candidates[index].clone(),
            reason,
            candidates_seen: candidates.len(),
        };
        let adapter = &adapters[index];
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|error| GpuError::DeviceRequest(error.to_string()))?;
        Ok(Self {
            device,
            queue,
            selection,
        })
    }

    /// Logical device on the selected adapter.
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Queue that executes command buffers on the device.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Which adapter was selected and why.
    #[must_use]
    pub fn selection(&self) -> &AdapterSelection {
        &self.selection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(device_type: wgpu::DeviceType) -> AdapterCandidate {
        AdapterCandidate {
            name: format!("{device_type:?} adapter"),
            backend: wgpu::Backend::Vulkan,
            device_type,
            vendor: 0,
            device: 0,
            driver: String::new(),
        }
    }

    #[test]
    fn ranks_follow_the_fallback_chain() {
        assert_eq!(adapter_rank(wgpu::DeviceType::DiscreteGpu), 0);
        assert_eq!(adapter_rank(wgpu::DeviceType::IntegratedGpu), 1);
        assert_eq!(adapter_rank(wgpu::DeviceType::VirtualGpu), 2);
        assert_eq!(adapter_rank(wgpu::DeviceType::Cpu), 3);
        assert_eq!(adapter_rank(wgpu::DeviceType::Other), 4);
    }

    #[test]
    fn selection_prefers_discrete_over_integrated_over_cpu() -> Result<(), String> {
        let candidates = vec![
            candidate(wgpu::DeviceType::Cpu),
            candidate(wgpu::DeviceType::IntegratedGpu),
            candidate(wgpu::DeviceType::DiscreteGpu),
        ];
        let (index, reason) = select_adapter(&candidates).ok_or("a non-empty list selects one")?;
        assert_eq!(index, 2);
        assert!(reason.contains("rank 0"));
        assert!(reason.contains("DiscreteGpu"));
        Ok(())
    }

    #[test]
    fn selection_falls_back_to_cpu_when_it_is_all_there_is() -> Result<(), String> {
        let candidates = vec![candidate(wgpu::DeviceType::Cpu)];
        let (index, reason) = select_adapter(&candidates).ok_or("a non-empty list selects one")?;
        assert_eq!(index, 0);
        assert!(reason.contains("rank 3"));
        Ok(())
    }

    #[test]
    fn selection_is_empty_with_no_candidates_and_stable_on_ties() -> Result<(), String> {
        assert!(select_adapter(&[]).is_none());
        let candidates = vec![
            candidate(wgpu::DeviceType::IntegratedGpu),
            candidate(wgpu::DeviceType::IntegratedGpu),
        ];
        let (index, _) = select_adapter(&candidates).ok_or("a non-empty list selects one")?;
        assert_eq!(index, 0);
        Ok(())
    }

    #[test]
    fn context_creation_matches_the_probe() {
        // The probe and the constructor enumerate the same
        // adapters, so they agree: a context exists exactly
        // when the probe is non-empty, and a failed creation
        // on an empty probe is `NoAdapter`, never another
        // error. This holds on a machine with a GPU and on
        // one without.
        let adapters = GpuContext::probe_adapters();
        let context = GpuContext::new();
        if adapters.is_empty() {
            assert!(matches!(context, Err(GpuError::NoAdapter)));
        } else {
            assert!(context.is_ok());
            if let Ok(context) = context {
                assert_eq!(context.selection().candidates_seen(), adapters.len());
            }
        }
    }
}
