//! The algorithm registry: discovery, inspection, validation,
//! instantiation, and graph construction for every registered
//! algorithm.
//!
//! The registry holds one [`AlgorithmFactory`] per algorithm.
//! The application discovers algorithms here, inspects their
//! [`AlgorithmDescriptor`], validates a configuration against
//! it, instantiates a processor or a set of graph nodes from
//! it, and runs the result through the normal processing graph
//! — there is no per-algorithm match arm anywhere in the
//! application, and no separate execution path. Adding an
//! algorithm means implementing the contract and calling
//! [`AlgorithmRegistry::register`]; capture, presentation, and
//! CLI code do not change. The integration boundary is
//! source-level (a Rust crate interface): there is no dynamic
//! plugin ABI.

use crate::algorithms::bicubic::BicubicAlgorithm;
use crate::algorithms::bilinear::BilinearAlgorithm;
use crate::algorithms::contour::ContourAlgorithm;
use crate::algorithms::epx::EpxAlgorithm;
use crate::algorithms::lanczos3::Lanczos3Algorithm;
use crate::algorithms::nearest::NearestAlgorithm;
use crate::algorithms::pixelflow::PixelFlowAlgorithm;
use crate::algorithms::scale2x::Scale2xAlgorithm;
use pixelflow_core::{
    AlgorithmConfig, AlgorithmDescriptor, AlgorithmError, AlgorithmFactory, Graph, GraphBuilder,
    Node, Source,
};

/// The registered algorithms, in registration order.
pub struct AlgorithmRegistry {
    /// One factory per registered algorithm.
    factories: Vec<Box<dyn AlgorithmFactory>>,
}

impl AlgorithmRegistry {
    /// An empty registry: no algorithm is registered until the
    /// caller registers one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            factories: Vec::new(),
        }
    }

    /// The registry with every built-in algorithm registered:
    /// Contour, Nearest, Bilinear, Bicubic, Lanczos3, Scale2x,
    /// Epx, and `PixelFlow`.
    #[must_use]
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(ContourAlgorithm));
        registry.register(Box::new(NearestAlgorithm));
        registry.register(Box::new(BilinearAlgorithm));
        registry.register(Box::new(BicubicAlgorithm));
        registry.register(Box::new(Lanczos3Algorithm));
        registry.register(Box::new(Scale2xAlgorithm));
        registry.register(Box::new(EpxAlgorithm));
        registry.register(Box::new(PixelFlowAlgorithm));
        registry
    }

    /// Register one algorithm. A later registration with the
    /// same id replaces the earlier one, so an external
    /// algorithm can supersede a built-in under its own id.
    pub fn register(&mut self, factory: Box<dyn AlgorithmFactory>) {
        let id = factory.descriptor().id;
        if let Some(slot) = self
            .factories
            .iter_mut()
            .find(|candidate| candidate.descriptor().id == id)
        {
            *slot = factory;
        } else {
            self.factories.push(factory);
        }
    }

    /// Ids of the registered algorithms, in registration order.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.factories
            .iter()
            .map(|factory| factory.descriptor().id)
            .collect()
    }

    /// Descriptors of the registered algorithms, in
    /// registration order.
    #[must_use]
    pub fn descriptors(&self) -> Vec<AlgorithmDescriptor> {
        self.factories
            .iter()
            .map(|factory| factory.descriptor())
            .collect()
    }

    /// The factory for `id`.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnknownAlgorithm`], listing
    /// the registered ids, when no algorithm with that id is
    /// registered.
    pub fn lookup(&self, id: &str) -> Result<&dyn AlgorithmFactory, AlgorithmError> {
        self.factories
            .iter()
            .find(|factory| factory.descriptor().id == id)
            .map(AsRef::as_ref)
            .ok_or_else(|| AlgorithmError::UnknownAlgorithm {
                id: id.to_string(),
                available: self.ids().into_iter().map(str::to_string).collect(),
            })
    }

    /// The descriptor for `id`.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnknownAlgorithm`], listing
    /// the registered ids, when no algorithm with that id is
    /// registered.
    pub fn descriptor(&self, id: &str) -> Result<AlgorithmDescriptor, AlgorithmError> {
        Ok(self.lookup(id)?.descriptor())
    }

    /// Build a processor for `id` from `config`.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnknownAlgorithm`] for an
    /// unregistered id, or [`AlgorithmError::InvalidConfig`]
    /// when the configuration fails validation.
    pub fn create_processor(
        &self,
        id: &str,
        config: &AlgorithmConfig,
    ) -> Result<Box<dyn pixelflow_core::Processor>, AlgorithmError> {
        self.lookup(id)?.create(config)
    }

    /// Build the graph nodes for `id` at `scale`, in run
    /// order, one node per stage the algorithm declares.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnknownAlgorithm`] for an
    /// unregistered id, [`AlgorithmError::UnsupportedScale`]
    /// for an unsupported scale, or
    /// [`AlgorithmError::InvalidConfig`] when the configuration
    /// fails validation.
    pub fn create_nodes(
        &self,
        id: &str,
        config: &AlgorithmConfig,
        scale: u32,
    ) -> Result<Vec<Box<dyn Node>>, AlgorithmError> {
        self.lookup(id)?.create_nodes(config, scale)
    }

    /// Build a processing graph for `id` at `scale`: one graph
    /// input ("in") feeding the algorithm's stages in order,
    /// the last stage's output exposed as the graph output
    /// ("out"). The graph is the normal graph — it validates
    /// ports, formats, and cycles like any other — and runs
    /// through the normal executor. A caller that routes
    /// branches builds its own graph from
    /// [`AlgorithmRegistry::create_nodes`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnknownAlgorithm`] for an
    /// unregistered id, [`AlgorithmError::UnsupportedScale`]
    /// for an unsupported scale,
    /// [`AlgorithmError::InvalidConfig`] when the configuration
    /// fails validation, or [`AlgorithmError::GraphBuild`] when
    /// the wired graph fails validation.
    pub fn build_graph(
        &self,
        id: &str,
        config: &AlgorithmConfig,
        scale: u32,
    ) -> Result<Graph, AlgorithmError> {
        let factory = self.lookup(id)?;
        let descriptor = factory.descriptor();
        let nodes = factory.create_nodes(config, scale)?;
        if nodes.len() != descriptor.stages.len() || nodes.is_empty() {
            return Err(AlgorithmError::GraphBuild(format!(
                "{} declares {} stages but built {} nodes",
                id,
                descriptor.stages.len(),
                nodes.len()
            )));
        }
        let input_format =
            descriptor.input_formats.first().copied().ok_or_else(|| {
                AlgorithmError::GraphBuild(format!("{id} declares no input format"))
            })?;
        let mut builder = GraphBuilder::new();
        // The builder methods return `&mut Self`; the chain is
        // carried in `current` and moved out with `take` at the
        // end, the same shape as the graph tests.
        let mut current = builder.add_input("in", input_format);
        for (stage, node) in descriptor.stages.iter().zip(nodes) {
            current = current.add_node(stage, node);
        }
        let first = descriptor
            .stages
            .first()
            .ok_or_else(|| AlgorithmError::GraphBuild(format!("{id} declares no stages")))?;
        current = current.connect(Source::input("in"), first, "in");
        for pair in descriptor.stages.windows(2) {
            let (from, to) = (pair[0], pair[1]);
            current = current.connect(Source::node(from, "out"), to, "in");
        }
        let last = descriptor
            .stages
            .last()
            .ok_or_else(|| AlgorithmError::GraphBuild(format!("{id} declares no stages")))?;
        current = current.add_output("out", last, "out");
        std::mem::take(current)
            .build()
            .map_err(|error| AlgorithmError::GraphBuild(error.to_string()))
    }
}

impl Default for AlgorithmRegistry {
    /// The built-in registry: every built-in algorithm.
    fn default() -> Self {
        Self::builtin()
    }
}
