//! Processing graph: nodes, edges, and the executor that runs
//! them.
//!
//! A graph is a directed acyclic network of processing nodes.
//! One node reads a frame and writes a frame; the graph is what
//! lets a frame take branches: one input feeding two nodes, one
//! node consuming two inputs, the original frame kept on one
//! branch while another branch reconstructs it. The executor
//! validates the graph before running (port names exist, every
//! input is fed exactly once, connected formats match, no
//! cycles), runs the nodes in topological order, propagates node
//! errors, and keeps an intermediate frame alive only as long
//! as a consumer still needs it.
//!
//! This step runs whole frames only. The executor's execution
//! path is the place dirty-region tracking hooks in: the next
//! step adds tile-level dirty sets, dependency propagation, and
//! per-node tile caches on top of the node declarations defined
//! here, without changing the graph structure or the node trait.

use crate::format::PixelFormat;
use crate::frame::{Frame, FrameError};
use crate::incremental::{DirtySet, TileGrid, integer_scale, propagate};
use crate::pipeline::{ProcessError, Processor, StrideSupport};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// What input area a node's output depends on, per the section
/// 9 specification. This is a fact about the implementation, not
/// a hint: if a node reads pixels outside its declared radius,
/// that is a bug, and the dependency-honesty tests catch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpatialDependency {
    /// One input pixel affects exactly one output pixel.
    Local,
    /// Output depends on a surrounding region: the radius in
    /// input pixels that can influence one output pixel. A 3x3
    /// convolution declares 1; a 5x5 declares 2; Contour's edge
    /// follower declares the radius its fitted runs reach.
    Neighborhood {
        /// Input pixels that can influence one output pixel.
        radius: u32,
    },
    /// Output depends on whole-frame statistics. Any input
    /// change invalidates the whole output. A node that cannot
    /// state its dependency honestly declares this: always
    /// correct, always slow.
    Global,
}

/// What history a node keeps between frames, per the section 9
/// specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemporalDependency {
    /// Output depends only on the current frame.
    Stateless,
    /// Output depends on previous frames. The node owns its
    /// history and states its own invalidation rule: which input
    /// changes discard which history.
    Stateful,
}

/// One processing node in a graph: a named set of input ports, a
/// named set of output ports, dependency declarations, and a
/// `run` that turns input frames into output frames.
pub trait Node {
    /// Short stable name, e.g. "contour" or "nearest". Used in
    /// configuration and in diagnostics; it need not be unique
    /// within a graph (the graph assigns each node its own name
    /// when it is added).
    fn name(&self) -> &'static str;

    /// Input port names, in the order `run` expects them.
    fn input_ports(&self) -> &[&'static str];

    /// Output port names.
    fn output_ports(&self) -> &[&'static str];

    /// Pixel format this input port accepts, or `None` for an
    /// undeclared port. The executor checks connected ports
    /// agree on format before running.
    fn input_format(&self, port: &str) -> Option<PixelFormat>;

    /// Pixel format this output port produces, or `None` for an
    /// undeclared port.
    fn output_format(&self, port: &str) -> Option<PixelFormat>;

    /// What input area this node's output depends on.
    fn spatial_dependency(&self) -> SpatialDependency;

    /// What history this node keeps between frames.
    fn temporal_dependency(&self) -> TemporalDependency;

    /// What row layouts this node accepts. The default is
    /// [`StrideSupport::Any`]; a tight-only node overrides this
    /// and the executor packs its input before running it.
    fn stride_support(&self) -> StrideSupport {
        StrideSupport::Any
    }

    /// Whether this node can run on a cropped input frame and
    /// produce exactly the corresponding crop of its
    /// whole-frame output. The default is `false`: the executor
    /// runs the node whole-frame and reads dirty tiles from that
    /// output. A node overrides this only when the cropped run
    /// is exact for its declared spatial dependency — a local
    /// node whose output pixel copies one input pixel qualifies;
    /// a node whose edge handling clamps to the frame boundary
    /// does not, because a crop would clamp to the crop
    /// boundary instead. The incremental executor only ever
    /// crops local tile-safe nodes; neighborhood and global
    /// nodes always run whole-frame when dirty.
    fn tile_safe(&self) -> bool {
        false
    }

    /// Run the node on its inputs and produce its outputs. Every
    /// declared input port is present in `inputs`; every
    /// declared output port must appear exactly once in the
    /// result.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError`] when the node cannot produce its
    /// outputs from the given inputs.
    fn run(&mut self, inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError>;
}

/// The frames a node reads, keyed by input port name. The
/// executor builds this per node run; a tight-only node's
/// inputs arrive already packed (see [`Frame::packed`]).
pub struct NodeInputs<'a> {
    /// Port name and frame, in the node's declared input order.
    frames: Vec<(&'static str, Cow<'a, Frame>)>,
}

impl NodeInputs<'_> {
    /// The frame on `port`, or `None` when the port was not
    /// supplied. The executor supplies every declared input
    /// port, so `None` here means a graph wiring bug, not a
    /// missing frame.
    #[must_use]
    pub fn get(&self, port: &str) -> Option<&Frame> {
        self.frames
            .iter()
            .find(|(name, _)| *name == port)
            .map(|(_, frame)| &**frame)
    }
}

/// The frames a node writes, keyed by output port name.
#[derive(Default)]
pub struct NodeOutputs {
    /// Port name and frame, in the order the node added them.
    frames: Vec<(&'static str, Frame)>,
}

impl NodeOutputs {
    /// An empty set of outputs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one output frame on `port`. The executor rejects a
    /// second frame on the same port and a frame on an
    /// undeclared port.
    pub fn add(&mut self, port: &'static str, frame: Frame) {
        self.frames.push((port, frame));
    }

    /// The output frames, in the order they were added.
    #[must_use]
    pub fn into_frames(self) -> Vec<(&'static str, Frame)> {
        self.frames
    }
}

/// A [`Processor`] wrapped as a graph node: one input port
/// ("in"), one output port ("out"), RGB8 on both. The spatial
/// dependency is supplied at construction because the
/// `Processor` trait does not declare one; the algorithm module
/// that builds the node states the honest value. In
/// `pixelflow-processing`, `Contour::node` and `Nearest::node`
/// are the only constructors callers should use for those
/// algorithms.
pub struct ProcessorNode<P: Processor> {
    /// The wrapped algorithm.
    processor: P,
    /// Scale factor passed to [`Processor::process`].
    scale: u32,
    /// The algorithm's spatial dependency, stated by its module.
    spatial: SpatialDependency,
}

impl<P: Processor> ProcessorNode<P> {
    /// Wrap `processor` as a node that runs it at `scale` and
    /// declares `spatial` as its spatial dependency.
    #[must_use]
    pub fn new(processor: P, scale: u32, spatial: SpatialDependency) -> Self {
        Self {
            processor,
            scale,
            spatial,
        }
    }
}

impl ProcessorNode<Box<dyn Processor>> {
    /// Wrap a type-erased processor as a node that runs it at
    /// `scale`, declaring the spatial dependency the processor
    /// itself states. The registry builds nodes this way, so no
    /// per-algorithm match arm is needed to wrap a processor.
    #[must_use]
    pub fn from_processor(processor: Box<dyn Processor>, scale: u32) -> Self {
        let spatial = processor.spatial_dependency();
        Self::new(processor, scale, spatial)
    }
}

impl<P: Processor> Node for ProcessorNode<P> {
    fn name(&self) -> &'static str {
        self.processor.name()
    }

    fn input_ports(&self) -> &[&'static str] {
        &["in"]
    }

    fn output_ports(&self) -> &[&'static str] {
        &["out"]
    }

    fn input_format(&self, port: &str) -> Option<PixelFormat> {
        if port == "in" {
            Some(PixelFormat::Rgb8)
        } else {
            None
        }
    }

    fn output_format(&self, port: &str) -> Option<PixelFormat> {
        if port == "out" {
            Some(PixelFormat::Rgb8)
        } else {
            None
        }
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        self.spatial
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    fn stride_support(&self) -> StrideSupport {
        self.processor.stride_support()
    }

    fn tile_safe(&self) -> bool {
        self.processor.tile_safe()
    }

    fn run(&mut self, inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError> {
        let input = inputs.get("in").ok_or_else(|| {
            ProcessError::Internal("processor node ran without its input frame".to_string())
        })?;
        let output = self.processor.process(input, self.scale)?;
        let mut outputs = NodeOutputs::new();
        outputs.add("out", output);
        Ok(outputs)
    }
}

/// One end of an edge: where a frame comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A graph input port, named at [`GraphBuilder::add_input`].
    Input(String),
    /// A node's output port.
    Node {
        /// Graph name of the producing node.
        node: String,
        /// Output port on that node.
        port: &'static str,
    },
}

impl Source {
    /// A graph input port as an edge source.
    #[must_use]
    pub fn input(name: &str) -> Self {
        Self::Input(name.to_string())
    }

    /// A node's output port as an edge source.
    #[must_use]
    pub fn node(node: &str, port: &'static str) -> Self {
        Self::Node {
            node: node.to_string(),
            port,
        }
    }
}

/// One edge: a frame flows from `from` to one input port of
/// `to_node`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Edge {
    /// Where the frame comes from.
    from: Source,
    /// Graph name of the consuming node.
    to_node: String,
    /// Input port on the consuming node.
    to_port: &'static str,
}

/// A slot in the executor's frame store: either a graph input
/// port or one node's output port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Slot {
    /// A graph input port.
    GraphInput(String),
    /// One node's output port, by node index in the graph.
    NodeOutput {
        /// Index of the producing node.
        node: usize,
        /// Output port on that node.
        port: &'static str,
    },
}

/// What can go wrong while building or running a graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// Two nodes share one graph name.
    DuplicateNode(String),
    /// Two graph inputs share one name.
    DuplicateInput(String),
    /// An edge or output names a node that is not in the graph.
    UnknownNode(String),
    /// An edge or output names a port the node does not declare.
    UnknownPort {
        /// Graph name of the node.
        node: String,
        /// Port name that was used.
        port: String,
    },
    /// An edge names a graph input that was never declared.
    UnknownGraphInput(String),
    /// `run` was given a frame for a graph input that was never
    /// declared.
    UnexpectedGraphInput(String),
    /// `run` was given two frames for one graph input.
    DuplicateGraphInput(String),
    /// `run` was not given a frame for a declared graph input.
    MissingGraphInput(String),
    /// A frame supplied to `run` has a different format than the
    /// graph input declares.
    InputFormatMismatch {
        /// Graph input port name.
        input: String,
        /// Format the graph input declares.
        expected: PixelFormat,
        /// Format of the frame that was supplied.
        found: PixelFormat,
    },
    /// A node's input port has no edge feeding it.
    InputUnconnected {
        /// Graph name of the node.
        node: String,
        /// Input port with no edge.
        port: String,
    },
    /// A node's input port has more than one edge feeding it.
    InputAlreadyConnected {
        /// Graph name of the node.
        node: String,
        /// Input port with two edges.
        port: String,
    },
    /// Two connected ports disagree on pixel format.
    MismatchedFormat {
        /// Name of the producing side (node name or graph input).
        from: String,
        /// Producing port.
        from_port: String,
        /// Graph name of the consuming node.
        to: String,
        /// Consuming port.
        to_port: String,
        /// Format the consuming port accepts.
        expected: PixelFormat,
        /// Format the producing port supplies.
        found: PixelFormat,
    },
    /// The node edges form a cycle; the listed nodes could not
    /// be ordered.
    Cycle(Vec<String>),
    /// A node failed while running.
    NodeFailed {
        /// Graph name of the node.
        node: String,
        /// The node's error.
        source: ProcessError,
    },
    /// A node did not produce one of its declared output ports.
    NodeOutputMissing {
        /// Graph name of the node.
        node: String,
        /// Output port that was not produced.
        port: String,
    },
    /// A node produced a frame on a port it does not declare.
    NodeOutputUnknown {
        /// Graph name of the node.
        node: String,
        /// Output port that was produced.
        port: String,
    },
    /// A node produced two frames on one output port.
    NodeOutputDuplicate {
        /// Graph name of the node.
        node: String,
        /// Output port produced twice.
        port: String,
    },
    /// An executor invariant broke; the message says what was
    /// expected. This is never a user error.
    Internal(String),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::DuplicateNode(name) => write!(f, "duplicate node name {name}"),
            GraphError::DuplicateInput(name) => write!(f, "duplicate graph input {name}"),
            GraphError::UnknownNode(name) => write!(f, "unknown node {name}"),
            GraphError::UnknownPort { node, port } => {
                write!(f, "node {node} has no port {port}")
            }
            GraphError::UnknownGraphInput(name) => write!(f, "unknown graph input {name}"),
            GraphError::UnexpectedGraphInput(name) => {
                write!(f, "unexpected graph input {name}")
            }
            GraphError::DuplicateGraphInput(name) => {
                write!(f, "graph input {name} supplied twice")
            }
            GraphError::MissingGraphInput(name) => write!(f, "missing graph input {name}"),
            GraphError::InputFormatMismatch {
                input,
                expected,
                found,
            } => write!(f, "graph input {input} expects {expected}, got {found}"),
            GraphError::InputUnconnected { node, port } => {
                write!(f, "input {port} of node {node} is not connected")
            }
            GraphError::InputAlreadyConnected { node, port } => {
                write!(f, "input {port} of node {node} is connected twice")
            }
            GraphError::MismatchedFormat {
                from,
                from_port,
                to,
                to_port,
                expected,
                found,
            } => write!(
                f,
                "port {from_port} of {from} supplies {found}, but port {to_port} of {to} expects {expected}"
            ),
            GraphError::Cycle(nodes) => write!(f, "cycle in graph nodes: {}", nodes.join(", ")),
            GraphError::NodeFailed { node, source } => write!(f, "node {node} failed: {source}"),
            GraphError::NodeOutputMissing { node, port } => {
                write!(f, "node {node} did not produce output {port}")
            }
            GraphError::NodeOutputUnknown { node, port } => {
                write!(f, "node {node} produced undeclared output {port}")
            }
            GraphError::NodeOutputDuplicate { node, port } => {
                write!(f, "node {node} produced output {port} twice")
            }
            GraphError::Internal(message) => write!(f, "graph executor error: {message}"),
        }
    }
}

impl std::error::Error for GraphError {}

/// Builds a [`Graph`]: nodes, edges, graph inputs, graph
/// outputs. [`GraphBuilder::build`] validates the whole graph
/// and returns the error instead of a graph when anything is
/// wrong.
#[derive(Default)]
pub struct GraphBuilder {
    /// Graph name and node, in the order added.
    nodes: Vec<(String, Box<dyn Node>)>,
    /// Edges, in the order added.
    edges: Vec<Edge>,
    /// Graph input port name and declared format.
    inputs: Vec<(String, PixelFormat)>,
    /// Graph output name, producing node name, producing port.
    outputs: Vec<(String, String, &'static str)>,
    /// Branch name and the node that processes it, in the
    /// order routed. A branch routed twice takes the last
    /// assignment.
    branch_nodes: Vec<(&'static str, String)>,
    /// Node that processes the general path, when one is
    /// routed.
    general_node: Option<String>,
}

impl GraphBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a node under `name`. Names are unique within a graph
    /// and are what edges and outputs refer to; they need not
    /// match the node's own [`Node::name`].
    #[must_use]
    pub fn add_node(&mut self, name: &str, node: Box<dyn Node>) -> &mut Self {
        self.nodes.push((name.to_string(), node));
        self
    }

    /// Declare a graph input port with the pixel format frames
    /// supplied for it must have.
    #[must_use]
    pub fn add_input(&mut self, name: &str, format: PixelFormat) -> &mut Self {
        self.inputs.push((name.to_string(), format));
        self
    }

    /// Connect `from` to input port `to_port` of node `to_node`.
    #[must_use]
    pub fn connect(&mut self, from: Source, to_node: &str, to_port: &'static str) -> &mut Self {
        self.edges.push(Edge {
            from,
            to_node: to_node.to_string(),
            to_port,
        });
        self
    }

    /// Name a graph output: the frame produced on `from_port` of
    /// node `from_node`, exposed under `name`.
    #[must_use]
    pub fn add_output(
        &mut self,
        name: &str,
        from_node: &str,
        from_port: &'static str,
    ) -> &mut Self {
        self.outputs
            .push((name.to_string(), from_node.to_string(), from_port));
        self
    }

    /// Route a branch to a node: tiles whose region class maps
    /// to `branch` are processed by `node`. A branch with no
    /// route falls back to the general path at run time. The
    /// node must exist in the graph; [`GraphBuilder::build`]
    /// reports it otherwise.
    #[must_use]
    pub fn route_branch(&mut self, branch: &'static str, node: &str) -> &mut Self {
        self.branch_nodes.push((branch, node.to_string()));
        self
    }

    /// Route the general path to a node: tiles with no branch,
    /// and tiles on a branch with no route, are processed by
    /// `node`. The node must exist in the graph;
    /// [`GraphBuilder::build`] reports it otherwise.
    #[must_use]
    pub fn route_general(&mut self, node: &str) -> &mut Self {
        self.general_node = Some(node.to_string());
        self
    }

    /// Validate the graph and return it, or return the first
    /// validation error found.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError`] for a duplicate name, an unknown
    /// node or port, an unconnected or twice-connected input, a
    /// format mismatch, or a cycle.
    pub fn build(self) -> Result<Graph, GraphError> {
        let input_names = self.validate_names()?;
        self.validate_edges(&input_names)?;
        self.validate_feeds()?;
        self.validate_formats()?;
        self.validate_outputs()?;
        self.validate_routes()?;
        // Topological order; a cycle leaves nodes unordered.
        let order = topological_order(&self.nodes, &self.edges)?;
        let feed = self.build_feed()?;
        let consumers = self.count_consumers()?;
        let graph_outputs = self.build_outputs()?;
        Ok(Graph {
            nodes: self.nodes,
            inputs: self.inputs,
            feed,
            consumers,
            outputs: graph_outputs,
            order,
            prev_inputs: HashMap::new(),
            node_caches: HashMap::new(),
            branch_nodes: self.branch_nodes.into_iter().collect(),
            general_node: self.general_node,
            routing: None,
            routing_invalidated: None,
        })
    }

    /// Index of the node named `name`, or `None`.
    fn node_index(&self, name: &str) -> Option<usize> {
        self.nodes
            .iter()
            .position(|(candidate, _)| candidate == name)
    }

    /// Node and graph-input names are unique. Returns the set
    /// of graph-input names for the edge check.
    fn validate_names(&self) -> Result<HashSet<&str>, GraphError> {
        let mut names: HashSet<&str> = HashSet::new();
        for (name, _) in &self.nodes {
            if !names.insert(name.as_str()) {
                return Err(GraphError::DuplicateNode(name.clone()));
            }
        }
        let mut input_names: HashSet<&str> = HashSet::new();
        for (name, _) in &self.inputs {
            if !input_names.insert(name.as_str()) {
                return Err(GraphError::DuplicateInput(name.clone()));
            }
        }
        Ok(input_names)
    }

    /// Every edge endpoint names a real node and port.
    fn validate_edges(&self, input_names: &HashSet<&str>) -> Result<(), GraphError> {
        for edge in &self.edges {
            let to = self
                .node_index(&edge.to_node)
                .ok_or_else(|| GraphError::UnknownNode(edge.to_node.clone()))?;
            let (_, to_node) = &self.nodes[to];
            if !to_node.input_ports().contains(&edge.to_port) {
                return Err(GraphError::UnknownPort {
                    node: edge.to_node.clone(),
                    port: edge.to_port.to_string(),
                });
            }
            match &edge.from {
                Source::Input(name) => {
                    if !input_names.contains(name.as_str()) {
                        return Err(GraphError::UnknownGraphInput(name.clone()));
                    }
                }
                Source::Node { node, port } => {
                    let from = self
                        .node_index(node)
                        .ok_or_else(|| GraphError::UnknownNode(node.clone()))?;
                    let (_, from_node) = &self.nodes[from];
                    if !from_node.output_ports().contains(port) {
                        return Err(GraphError::UnknownPort {
                            node: node.clone(),
                            port: (*port).to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Every input port is fed exactly once.
    fn validate_feeds(&self) -> Result<(), GraphError> {
        for edge in &self.edges {
            let same_target = self
                .edges
                .iter()
                .filter(|other| other.to_node == edge.to_node && other.to_port == edge.to_port)
                .count();
            if same_target > 1 {
                return Err(GraphError::InputAlreadyConnected {
                    node: edge.to_node.clone(),
                    port: edge.to_port.to_string(),
                });
            }
        }
        for (name, node) in &self.nodes {
            for port in node.input_ports() {
                let fed = self
                    .edges
                    .iter()
                    .any(|edge| edge.to_node == *name && edge.to_port == *port);
                if !fed {
                    return Err(GraphError::InputUnconnected {
                        node: name.clone(),
                        port: (*port).to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Connected ports agree on pixel format.
    fn validate_formats(&self) -> Result<(), GraphError> {
        for edge in &self.edges {
            let (_, to_node) = &self.nodes[self
                .node_index(&edge.to_node)
                .ok_or_else(|| GraphError::Internal(format!("lost node {}", edge.to_node)))?];
            let expected = to_node.input_format(edge.to_port).ok_or_else(|| {
                GraphError::Internal(format!(
                    "node {} does not declare a format for input {}",
                    edge.to_node, edge.to_port
                ))
            })?;
            let (from_name, from_port, found) = match &edge.from {
                Source::Input(name) => {
                    let format = self
                        .inputs
                        .iter()
                        .find(|(candidate, _)| candidate == name)
                        .map(|(_, format)| *format)
                        .ok_or_else(|| GraphError::UnknownGraphInput(name.clone()))?;
                    (name.clone(), String::new(), format)
                }
                Source::Node { node, port } => {
                    let from = self
                        .node_index(node)
                        .ok_or_else(|| GraphError::UnknownNode(node.clone()))?;
                    let (_, from_node) = &self.nodes[from];
                    let format = from_node.output_format(port).ok_or_else(|| {
                        GraphError::Internal(format!(
                            "node {node} does not declare a format for output {port}"
                        ))
                    })?;
                    (node.clone(), (*port).to_string(), format)
                }
            };
            if found != expected {
                return Err(GraphError::MismatchedFormat {
                    from: from_name,
                    from_port,
                    to: edge.to_node.clone(),
                    to_port: edge.to_port.to_string(),
                    expected,
                    found,
                });
            }
        }
        Ok(())
    }

    /// Every graph output names a real node and port.
    fn validate_outputs(&self) -> Result<(), GraphError> {
        for (_, node, port) in &self.outputs {
            let index = self
                .node_index(node)
                .ok_or_else(|| GraphError::UnknownNode(node.clone()))?;
            let (_, output_node) = &self.nodes[index];
            if !output_node.output_ports().contains(port) {
                return Err(GraphError::UnknownPort {
                    node: node.clone(),
                    port: (*port).to_string(),
                });
            }
        }
        Ok(())
    }

    /// Every routed branch and the general path name a node
    /// in the graph.
    fn validate_routes(&self) -> Result<(), GraphError> {
        for (_, node) in &self.branch_nodes {
            if self.node_index(node).is_none() {
                return Err(GraphError::UnknownNode(node.clone()));
            }
        }
        if let Some(node) = &self.general_node
            && self.node_index(node).is_none()
        {
            return Err(GraphError::UnknownNode(node.clone()));
        }
        Ok(())
    }

    /// Per node, per input port in declaration order, the slot
    /// that feeds it.
    fn build_feed(&self) -> Result<Vec<Vec<(Slot, &'static str)>>, GraphError> {
        let mut feed: Vec<Vec<(Slot, &'static str)>> = Vec::with_capacity(self.nodes.len());
        for (name, node) in &self.nodes {
            let mut node_feed: Vec<(Slot, &'static str)> =
                Vec::with_capacity(node.input_ports().len());
            for port in node.input_ports() {
                let edge = self
                    .edges
                    .iter()
                    .find(|edge| edge.to_node == *name && edge.to_port == *port)
                    .ok_or_else(|| GraphError::InputUnconnected {
                        node: name.clone(),
                        port: (*port).to_string(),
                    })?;
                let slot = match &edge.from {
                    Source::Input(input) => Slot::GraphInput(input.clone()),
                    Source::Node { node, port } => Slot::NodeOutput {
                        node: self
                            .node_index(node)
                            .ok_or_else(|| GraphError::UnknownNode(node.clone()))?,
                        port,
                    },
                };
                node_feed.push((slot, *port));
            }
            feed.push(node_feed);
        }
        Ok(feed)
    }

    /// How many consumers still need each slot. A slot drops
    /// out of the executor's store when its count reaches
    /// zero, so an intermediate frame lives only as long as a
    /// consumer needs it. Graph outputs count as consumers of
    /// their slot, so an output frame survives until the run
    /// collects it.
    fn count_consumers(&self) -> Result<HashMap<Slot, usize>, GraphError> {
        let mut consumers: HashMap<Slot, usize> = HashMap::new();
        for edge in &self.edges {
            let slot = match &edge.from {
                Source::Input(name) => Slot::GraphInput(name.clone()),
                Source::Node { node, port } => Slot::NodeOutput {
                    node: self
                        .node_index(node)
                        .ok_or_else(|| GraphError::UnknownNode(node.clone()))?,
                    port,
                },
            };
            *consumers.entry(slot).or_insert(0) += 1;
        }
        for (_, node, port) in &self.outputs {
            let slot = Slot::NodeOutput {
                node: self
                    .node_index(node)
                    .ok_or_else(|| GraphError::UnknownNode(node.clone()))?,
                port,
            };
            *consumers.entry(slot).or_insert(0) += 1;
        }
        Ok(consumers)
    }

    /// Graph outputs as (name, slot) pairs, in declaration
    /// order.
    fn build_outputs(&self) -> Result<Vec<(String, Slot)>, GraphError> {
        let mut graph_outputs: Vec<(String, Slot)> = Vec::with_capacity(self.outputs.len());
        for (name, node, port) in &self.outputs {
            let slot = Slot::NodeOutput {
                node: self
                    .node_index(node)
                    .ok_or_else(|| GraphError::UnknownNode(node.clone()))?,
                port,
            };
            graph_outputs.push((name.clone(), slot));
        }
        Ok(graph_outputs)
    }
}

/// Topological order of the graph's nodes by node index, or a
/// [`GraphError::Cycle`] naming the nodes that could not be
/// ordered.
fn topological_order(
    nodes: &[(String, Box<dyn Node>)],
    edges: &[Edge],
) -> Result<Vec<usize>, GraphError> {
    let index_of =
        |name: &str| -> Option<usize> { nodes.iter().position(|(candidate, _)| candidate == name) };
    // In-degree per node, counting only node-to-node edges, and
    // the consumers of each node for Kahn's algorithm.
    let mut in_degree = vec![0usize; nodes.len()];
    let mut after: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for edge in edges {
        if let Source::Node { node, .. } = &edge.from {
            let from =
                index_of(node).ok_or_else(|| GraphError::Internal(format!("lost node {node}")))?;
            let to = index_of(&edge.to_node)
                .ok_or_else(|| GraphError::Internal(format!("lost node {}", edge.to_node)))?;
            in_degree[to] += 1;
            after[from].push(to);
        }
    }
    let mut ready: Vec<usize> = (0..nodes.len()).filter(|i| in_degree[*i] == 0).collect();
    let mut order: Vec<usize> = Vec::with_capacity(nodes.len());
    while let Some(node) = ready.pop() {
        order.push(node);
        for next in std::mem::take(&mut after[node]) {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                ready.push(next);
            }
        }
    }
    if order.len() != nodes.len() {
        let stuck: Vec<String> = (0..nodes.len())
            .filter(|i| !order.contains(i))
            .map(|i| nodes[i].0.clone())
            .collect();
        return Err(GraphError::Cycle(stuck));
    }
    Ok(order)
}

/// Per-tile routing assignment for one frame: which branch,
/// if any, each tile routes to. `None` is the general path.
/// The branch names are the routing profile's names, such as
/// `"text"`; the graph maps them to nodes with
/// [`GraphBuilder::route_branch`], and
/// [`Graph::node_for_tile`] is the decision point that reads
/// this table. The table is metadata only: it names where
/// each tile goes, and the executor runs the node it names.
/// With one node in the graph every branch resolves to it,
/// so the table changes no pixels until per-branch
/// processors exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileRouting {
    /// The tile grid these assignments index into.
    grid: TileGrid,
    /// Branch per tile, row-major; `None` is the general path.
    branches: Vec<Option<&'static str>>,
}

impl TileRouting {
    /// No tile routed to a branch: every tile takes the
    /// general path.
    #[must_use]
    pub fn empty(grid: TileGrid) -> Self {
        Self {
            grid,
            branches: vec![None; grid.tile_count()],
        }
    }

    /// The tile grid this routing indexes into.
    #[must_use]
    pub fn grid(&self) -> TileGrid {
        self.grid
    }

    /// The branch tile (`tx`, `ty`) routes to, or `None`
    /// when the tile takes the general path or is outside the
    /// grid. Callers that need the distinction check the grid
    /// first; [`Graph::node_for_tile`] does.
    #[must_use]
    pub fn branch_for(&self, tx: u32, ty: u32) -> Option<&'static str> {
        if tx >= self.grid.cols() || ty >= self.grid.rows() {
            return None;
        }
        self.branches[self.grid.tile_index(tx, ty)]
    }

    /// Route tile (`tx`, `ty`) to `branch`, or to the general
    /// path when `branch` is `None`. Out-of-range tiles are
    /// ignored.
    pub fn set_branch(&mut self, tx: u32, ty: u32, branch: Option<&'static str>) {
        if tx >= self.grid.cols() || ty >= self.grid.rows() {
            return;
        }
        let index = self.grid.tile_index(tx, ty);
        self.branches[index] = branch;
    }

    /// How many tiles route to a branch instead of the
    /// general path.
    #[must_use]
    pub fn routed_count(&self) -> usize {
        self.branches
            .iter()
            .filter(|branch| branch.is_some())
            .count()
    }
}

/// A validated processing graph, ready to run. Built only by
/// [`GraphBuilder::build`].
pub struct Graph {
    /// Graph name and node, in the order added.
    nodes: Vec<(String, Box<dyn Node>)>,
    /// Declared graph input port name and format. Kept so
    /// `run` can check supplied frames against the declared
    /// format instead of recovering the declarations from the
    /// edges.
    inputs: Vec<(String, PixelFormat)>,
    /// Per node, per input port in declaration order, the slot
    /// that feeds it.
    feed: Vec<Vec<(Slot, &'static str)>>,
    /// How many consumers still need each slot.
    consumers: HashMap<Slot, usize>,
    /// Graph output name and the slot that supplies it.
    outputs: Vec<(String, Slot)>,
    /// Node indexes in topological order.
    order: Vec<usize>,
    /// Previous frame per graph input port, for dirty-region
    /// detection in [`Graph::run_incremental`]. Empty until the
    /// first incremental run; cleared by [`Graph::run`], so a
    /// whole-frame run resets incremental state and the next
    /// incremental run starts from first-frame behavior.
    prev_inputs: HashMap<String, Frame>,
    /// Cached output frame per node-output slot: the per-node
    /// tile caches. A node's cache holds its last output frame
    /// in full; the dirty set recorded for the slot says which
    /// of its tiles the next run may treat as stale. Cleared by
    /// [`Graph::run`] together with `prev_inputs`.
    node_caches: HashMap<Slot, Frame>,
    /// Branch name and the node that processes it, from
    /// [`GraphBuilder::route_branch`].
    branch_nodes: HashMap<&'static str, String>,
    /// Node that processes the general path, from
    /// [`GraphBuilder::route_general`].
    general_node: Option<String>,
    /// Per-tile routing of the most recent frame handed to the
    /// graph with [`Graph::set_routing`], or `None` before the
    /// first routed frame. `run` and `run_incremental` leave
    /// it in place: it describes the frame just processed,
    /// and the caller refreshes it every frame.
    routing: Option<TileRouting>,
    /// Halo-expanded invalidation of the most recent routed
    /// frame, from [`Graph::set_routing`]: the pixel dirty set
    /// plus the halo around tiles that changed class. The
    /// executor does not consume it yet: with a single Global
    /// node the run is whole-frame regardless, and this set is
    /// the invalidation the per-branch executor will read.
    routing_invalidated: Option<DirtySet>,
}

impl Graph {
    /// Hand the graph one frame's routing: the per-tile
    /// branch assignment from the region map, and the
    /// halo-expanded dirty set for that frame. The routing
    /// decision point is [`Graph::node_for_tile`], which
    /// reads this table; call this before `run` for every
    /// frame, so the table never describes a stale frame.
    /// `run` leaves the table in place, so the routing of the
    /// frame just processed stays readable after the run.
    pub fn set_routing(&mut self, routing: TileRouting, invalidated: DirtySet) {
        self.routing = Some(routing);
        self.routing_invalidated = Some(invalidated);
    }

    /// The per-tile routing of the most recent routed frame,
    /// or `None` before the first routed frame.
    #[must_use]
    pub fn routing(&self) -> Option<&TileRouting> {
        self.routing.as_ref()
    }

    /// The halo-expanded invalidation of the most recent
    /// routed frame, or `None` before the first routed frame.
    #[must_use]
    pub fn routing_invalidated(&self) -> Option<&DirtySet> {
        self.routing_invalidated.as_ref()
    }

    /// The routing decision point: which node processes
    /// `branch`. A routed branch resolves to its node; an
    /// unrouted branch and the general path (`None`) resolve
    /// to the general node. `None` when no node is assigned
    /// for the branch: no general node was routed.
    #[must_use]
    pub fn node_for_branch(&self, branch: Option<&'static str>) -> Option<&str> {
        match branch {
            Some(name) => self
                .branch_nodes
                .get(name)
                .map(String::as_str)
                .or(self.general_node.as_deref()),
            None => self.general_node.as_deref(),
        }
    }

    /// The routing decision point, per tile: which node
    /// processes tile (`tx`, `ty`) under the current routing.
    /// `None` when no routing is set, when the tile is
    /// outside the grid, or when no node is assigned for the
    /// tile's branch.
    #[must_use]
    pub fn node_for_tile(&self, tx: u32, ty: u32) -> Option<&str> {
        let routing = self.routing.as_ref()?;
        if tx >= routing.grid.cols() || ty >= routing.grid.rows() {
            return None;
        }
        self.node_for_branch(routing.branch_for(tx, ty))
    }

    /// Run the graph on `inputs`, one owned frame per declared
    /// graph input port, and return the graph outputs in the
    /// order they were declared. Ownership of the input frames
    /// moves into the graph; no frame is cloned on the way in.
    /// A stateful node keeps its history between runs, so
    /// running the same graph twice on different frames is a
    /// sequence, not two isolated runs.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError`] for a missing, unexpected,
    /// duplicated, or wrongly formatted graph input, or when a
    /// node fails.
    pub fn run(&mut self, inputs: Vec<(&str, Frame)>) -> Result<Vec<(String, Frame)>, GraphError> {
        // A whole-frame run resets incremental state, so mixing
        // `run` and `run_incremental` on one graph stays safe:
        // the next incremental run starts from first-frame
        // behavior and rebuilds every cache.
        self.prev_inputs.clear();
        self.node_caches.clear();
        let mut slots = self.load_inputs(inputs)?;
        let mut remaining = self.consumers.clone();
        // Copy the order: executing a node borrows the graph
        // mutably, so the loop cannot borrow the order at the
        // same time. The list holds one index per node.
        let order = self.order.clone();
        for index in order {
            self.execute_node(&mut slots, &mut remaining, index)?;
        }
        let mut outputs: Vec<(String, Frame)> = Vec::with_capacity(self.outputs.len());
        for (name, slot) in &self.outputs {
            let frame = slots
                .remove(slot)
                .ok_or_else(|| GraphError::Internal(format!("graph output {name} has no frame")))?;
            outputs.push((name.clone(), frame));
        }
        Ok(outputs)
    }

    /// Run the graph incrementally on `inputs`: compare each
    /// input with the previous frame, propagate the dirty tiles
    /// through the graph by each node's declared dependency,
    /// skip nodes whose output dirty set is empty, and reuse
    /// their cached outputs. A tile-safe local node reruns only
    /// its dirty tiles, from cropped inputs; every other dirty
    /// node reruns whole-frame. The output is exactly the
    /// whole-frame output: cached tiles are byte-identical to
    /// what a fresh run would produce, because a tile leaves
    /// the dirty set only when no input pixel it depends on
    /// changed.
    ///
    /// The first incremental run has no previous frame, so
    /// every input is wholly dirty and every node runs
    /// whole-frame and fills its cache. A resolution or format
    /// change is wholly dirty the same way. A failed run leaves
    /// no partial state: the caches and previous frames are
    /// cleared, so the next run starts from first-frame
    /// behavior. Node parameters are fixed for the graph's
    /// lifetime — a caller that changes a node's parameters
    /// builds a new graph, since the executor has no hook that
    /// reports a parameter change. Graph outputs are stamped
    /// with the first graph input's timestamp and sequence, so
    /// a partially updated frame still carries the current
    /// frame's capture metadata.
    ///
    /// # Errors
    ///
    /// Returns [`GraphError`] for a missing, unexpected,
    /// duplicated, or wrongly formatted graph input, or when a
    /// node fails — the same errors as [`Graph::run`].
    pub fn run_incremental(
        &mut self,
        inputs: Vec<(&str, Frame)>,
    ) -> Result<Vec<(String, Frame)>, GraphError> {
        let current = self.load_inputs(inputs)?;
        let mut dirty: HashMap<Slot, DirtySet> = HashMap::new();
        for (name, _) in &self.inputs {
            let slot = Slot::GraphInput(name.clone());
            let frame = current
                .get(&slot)
                .ok_or_else(|| GraphError::Internal(format!("graph input {name} has no frame")))?;
            let set = match self.prev_inputs.get(name) {
                Some(prev) => DirtySet::diff(prev, frame),
                None => DirtySet::whole(TileGrid::from_dims(frame.width(), frame.height())),
            };
            dirty.insert(slot, set);
        }
        let order = self.order.clone();
        for index in order {
            let mut run = IncrementalRun {
                inputs: &current,
                caches: &mut self.node_caches,
                dirty: &mut dirty,
            };
            if let Err(error) =
                run_node_incremental(&mut run, &mut self.nodes[index], &self.feed[index], index)
            {
                // A failed incremental run leaves no partial
                // state: the next run starts from first-frame
                // behavior and rebuilds every cache.
                self.prev_inputs.clear();
                self.node_caches.clear();
                return Err(error);
            }
        }
        // Outputs are clones of the caches, stamped with the
        // first graph input's capture metadata. A graph with no
        // inputs has no metadata to stamp.
        let stamp = self.inputs.first().and_then(|(name, _)| {
            current
                .get(&Slot::GraphInput(name.clone()))
                .map(|frame| (frame.timestamp(), frame.sequence()))
        });
        let mut outputs: Vec<(String, Frame)> = Vec::with_capacity(self.outputs.len());
        for (name, slot) in &self.outputs {
            let frame = self
                .node_caches
                .get(slot)
                .ok_or_else(|| GraphError::Internal(format!("graph output {name} has no frame")))?;
            let mut frame = frame.clone();
            if let Some((timestamp, sequence)) = stamp {
                frame = frame.with_timestamp(timestamp).with_sequence(sequence);
            }
            outputs.push((name.clone(), frame));
        }
        // The current frames become the previous frames for the
        // next run's comparison.
        let mut prev: HashMap<String, Frame> = HashMap::new();
        for (slot, frame) in current {
            if let Slot::GraphInput(name) = slot {
                prev.insert(name, frame);
            }
        }
        self.prev_inputs = prev;
        Ok(outputs)
    }

    /// Move the supplied frames into slots, checking every
    /// declared graph input is present exactly once and in its
    /// declared format.
    fn load_inputs(&self, inputs: Vec<(&str, Frame)>) -> Result<HashMap<Slot, Frame>, GraphError> {
        let mut slots: HashMap<Slot, Frame> = HashMap::new();
        for (name, frame) in inputs {
            let (_, expected) = self
                .inputs
                .iter()
                .find(|(candidate, _)| candidate == name)
                .ok_or_else(|| GraphError::UnexpectedGraphInput(name.to_string()))?;
            if frame.format() != *expected {
                return Err(GraphError::InputFormatMismatch {
                    input: name.to_string(),
                    expected: *expected,
                    found: frame.format(),
                });
            }
            let slot = Slot::GraphInput(name.to_string());
            if slots.contains_key(&slot) {
                return Err(GraphError::DuplicateGraphInput(name.to_string()));
            }
            slots.insert(slot, frame);
        }
        for (name, _) in &self.inputs {
            if !slots.contains_key(&Slot::GraphInput(name.clone())) {
                return Err(GraphError::MissingGraphInput(name.clone()));
            }
        }
        Ok(slots)
    }

    /// Run one node: gather its inputs (packing strided frames
    /// for a tight-only node), run it, store its outputs, and
    /// drop any input slot whose last consumer this was. The
    /// executor's execution path is the place dirty-region
    /// tracking hooks in: the next step adds tile-level dirty
    /// sets and per-node caches around this call, without
    /// changing the graph structure or the node trait.
    fn execute_node(
        &mut self,
        slots: &mut HashMap<Slot, Frame>,
        remaining: &mut HashMap<Slot, usize>,
        index: usize,
    ) -> Result<(), GraphError> {
        let (name, node) = &mut self.nodes[index];
        let mut inputs = NodeInputs { frames: Vec::new() };
        for (slot, port) in &self.feed[index] {
            let frame = slots.get(slot).ok_or_else(|| {
                GraphError::Internal(format!("node {name} input {port} has no frame"))
            })?;
            let frame = if node.stride_support() == StrideSupport::TightOnly && !frame.is_tight() {
                Cow::Owned(frame.packed().into_owned())
            } else {
                Cow::Borrowed(frame)
            };
            inputs.frames.push((*port, frame));
        }
        let outputs = node.run(&inputs).map_err(|source| GraphError::NodeFailed {
            node: name.clone(),
            source,
        })?;
        drop(inputs);
        let produced = outputs.into_frames();
        let mut seen: HashSet<&'static str> = HashSet::new();
        for (port, frame) in produced {
            if !node.output_ports().contains(&port) {
                return Err(GraphError::NodeOutputUnknown {
                    node: name.clone(),
                    port: port.to_string(),
                });
            }
            if !seen.insert(port) {
                return Err(GraphError::NodeOutputDuplicate {
                    node: name.clone(),
                    port: port.to_string(),
                });
            }
            slots.insert(Slot::NodeOutput { node: index, port }, frame);
        }
        for port in node.output_ports() {
            if !seen.contains(port) {
                return Err(GraphError::NodeOutputMissing {
                    node: name.clone(),
                    port: (*port).to_string(),
                });
            }
        }
        for (slot, _) in &self.feed[index] {
            if let Some(count) = remaining.get_mut(slot) {
                *count -= 1;
                if *count == 0 {
                    slots.remove(slot);
                }
            }
        }
        Ok(())
    }
}

/// Shared state for one incremental node run: the current input
/// frames, the per-node output caches, and the per-slot dirty
/// sets. Grouped so the per-node helpers stay under the argument
/// limit.
struct IncrementalRun<'a> {
    inputs: &'a HashMap<Slot, Frame>,
    caches: &'a mut HashMap<Slot, Frame>,
    dirty: &'a mut HashMap<Slot, DirtySet>,
}

/// One tile-safe run: which output slot the tiles fill, which
/// of its tiles are dirty, the output tile grid, and the node's
/// integer scale from input to output.
struct TileRun<'a> {
    out_slot: &'a Slot,
    set: &'a DirtySet,
    grid: TileGrid,
    scale: u32,
}

/// One node's input for the incremental executor: the slot and
/// port it feeds, the input frame's dimensions, and the input's
/// dirty set. Dimensions and sets are owned, so no frame borrow
/// is held while the cache is mutated below.
type InputInfo = (Slot, &'static str, u32, u32, DirtySet);

/// One node's propagated output: the output slot, its dirty
/// set, and whether an output was cached at all. A port with no
/// cache yet runs whole-frame and its dirty set is the whole
/// fresh output, filled in after the run.
type PropagatedSet = (Slot, DirtySet, bool);

/// Gather one node's inputs: each feed slot's frame dimensions
/// and dirty set.
fn gather_input_infos(
    run: &IncrementalRun<'_>,
    name: &str,
    feed: &[(Slot, &'static str)],
) -> Result<Vec<InputInfo>, GraphError> {
    let mut infos: Vec<InputInfo> = Vec::with_capacity(feed.len());
    for (slot, port) in feed {
        let frame = run
            .inputs
            .get(slot)
            .or_else(|| run.caches.get(slot))
            .ok_or_else(|| {
                GraphError::Internal(format!("node {name} input {port} has no frame"))
            })?;
        let set = run
            .dirty
            .get(slot)
            .ok_or_else(|| {
                GraphError::Internal(format!("node {name} input {port} has no dirty set"))
            })?
            .clone();
        infos.push((slot.clone(), *port, frame.width(), frame.height(), set));
    }
    Ok(infos)
}

/// One propagated dirty set per output port, from the cached
/// output dims.
fn propagate_node_outputs(
    run: &IncrementalRun<'_>,
    node: &dyn Node,
    index: usize,
    input_infos: &[InputInfo],
) -> Vec<PropagatedSet> {
    let mut propagated: Vec<PropagatedSet> = Vec::with_capacity(node.output_ports().len());
    for port in node.output_ports() {
        let slot = Slot::NodeOutput { node: index, port };
        match run.caches.get(&slot) {
            Some(cached) => {
                let out_grid = TileGrid::from_dims(cached.width(), cached.height());
                let mut set = DirtySet::empty(out_grid);
                for (_, _, width, height, in_set) in input_infos {
                    let part = propagate(
                        node.spatial_dependency(),
                        node.temporal_dependency(),
                        in_set,
                        (*width, *height),
                        out_grid,
                    );
                    set = set.union(&part);
                }
                propagated.push((slot, set, true));
            }
            None => propagated.push((slot, DirtySet::empty(TileGrid::from_dims(0, 0)), false)),
        }
    }
    propagated
}

/// Run one node incrementally: propagate its input dirty sets
/// into one output dirty set per output port, skip the node
/// when every set is empty, rerun only dirty tiles for a
/// tile-safe local node, and otherwise rerun whole-frame. The
/// cached output and the per-slot dirty sets are updated, so
/// downstream nodes see exactly the tiles that may have
/// changed. The input gathering and output validation mirror
/// [`Graph::execute_node`]; the two stay separate because the
/// whole-frame executor drops slots when consumers finish,
/// while the incremental executor keeps caches across runs.
fn run_node_incremental(
    run: &mut IncrementalRun<'_>,
    node_entry: &mut (String, Box<dyn Node>),
    feed: &[(Slot, &'static str)],
    index: usize,
) -> Result<(), GraphError> {
    let (name, node) = node_entry;
    let input_infos = gather_input_infos(run, name, feed)?;
    let propagated = propagate_node_outputs(run, &**node, index, &input_infos);
    // Skip the node when every output is cached and clean: its
    // cached outputs already equal a fresh run's.
    if propagated
        .iter()
        .all(|(_, set, has_cache)| *has_cache && set.is_empty())
    {
        for (slot, set, _) in propagated {
            run.dirty.insert(slot, set);
        }
        return Ok(());
    }
    // A tile-safe local node with one input, one output, and an
    // integer scale reruns only its dirty tiles, from cropped
    // inputs. A wholly dirty output is cheaper whole-frame than
    // tile by tile, so it takes the whole-frame path below.
    if node.output_ports().len() == 1
        && feed.len() == 1
        && node.tile_safe()
        && node.spatial_dependency() == SpatialDependency::Local
        && node.temporal_dependency() == TemporalDependency::Stateless
    {
        let (slot, set, has_cache) = &propagated[0];
        if *has_cache && !set.is_empty() && set.count() < set.grid().tile_count() {
            let (_, _, in_width, in_height, _) = input_infos[0];
            let out_grid = set.grid();
            if let Some(scale) =
                integer_scale((in_width, in_height), (out_grid.width(), out_grid.height()))
            {
                let out_slot = slot.clone();
                let set = set.clone();
                let tile = TileRun {
                    out_slot: slot,
                    set: &set,
                    grid: out_grid,
                    scale,
                };
                run_node_tiles(run, name, node, feed, &tile)?;
                run.dirty.insert(out_slot, set);
                return Ok(());
            }
        }
    }
    run_node_whole_frame(run, name, node, feed, index, &propagated)
}

/// Whole-frame path: gather inputs (packing strided frames
/// for a tight-only node), run the node, validate its outputs
/// the same way `execute_node` does, and store them in the
/// caches.
fn run_node_whole_frame(
    run: &mut IncrementalRun<'_>,
    name: &str,
    node: &mut Box<dyn Node>,
    feed: &[(Slot, &'static str)],
    index: usize,
    propagated: &[PropagatedSet],
) -> Result<(), GraphError> {
    let produced = {
        let mut node_inputs = NodeInputs { frames: Vec::new() };
        for (slot, port) in feed {
            let frame = run
                .inputs
                .get(slot)
                .or_else(|| run.caches.get(slot))
                .ok_or_else(|| {
                    GraphError::Internal(format!("node {name} input {port} has no frame"))
                })?;
            let frame = if node.stride_support() == StrideSupport::TightOnly && !frame.is_tight() {
                Cow::Owned(frame.packed().into_owned())
            } else {
                Cow::Borrowed(frame)
            };
            node_inputs.frames.push((*port, frame));
        }
        let result = node
            .run(&node_inputs)
            .map_err(|source| GraphError::NodeFailed {
                node: name.to_string(),
                source,
            });
        drop(node_inputs);
        result?
    };
    let produced = produced.into_frames();
    let mut seen: HashSet<&'static str> = HashSet::new();
    for (port, frame) in produced {
        if !node.output_ports().contains(&port) {
            return Err(GraphError::NodeOutputUnknown {
                node: name.to_string(),
                port: port.to_string(),
            });
        }
        if !seen.insert(port) {
            return Err(GraphError::NodeOutputDuplicate {
                node: name.to_string(),
                port: port.to_string(),
            });
        }
        let slot = Slot::NodeOutput { node: index, port };
        let set = match propagated
            .iter()
            .find(|(candidate, _, _)| *candidate == slot)
        {
            Some((_, set, true)) => set.clone(),
            _ => DirtySet::whole(TileGrid::from_dims(frame.width(), frame.height())),
        };
        run.dirty.insert(slot.clone(), set);
        run.caches.insert(slot, frame);
    }
    for port in node.output_ports() {
        if !seen.contains(port) {
            return Err(GraphError::NodeOutputMissing {
                node: name.to_string(),
                port: (*port).to_string(),
            });
        }
    }
    Ok(())
}

/// Run a tile-safe local node once per dirty tile, on cropped
/// inputs, and splice the tile outputs into the cached output
/// frame. Two phases: the cropped runs borrow the input frame,
/// so they all happen first and their outputs are collected;
/// only then is the cache mutated, once no input borrow is
/// live.
fn run_node_tiles(
    run: &mut IncrementalRun<'_>,
    name: &str,
    node: &mut Box<dyn Node>,
    feed: &[(Slot, &'static str)],
    tile: &TileRun<'_>,
) -> Result<(), GraphError> {
    let in_slot = feed[0].0.clone();
    let in_port = feed[0].1;
    let out_port = match tile.out_slot {
        Slot::NodeOutput { port, .. } => *port,
        Slot::GraphInput(_) => {
            return Err(GraphError::Internal(format!(
                "node {name} tile run has a graph-input output slot"
            )));
        }
    };
    // Phase 1: run the node once per dirty tile on cropped
    // inputs, collecting the tile outputs.
    let tile_outputs: Vec<(u32, u32, Frame)> = {
        let in_frame = run
            .inputs
            .get(&in_slot)
            .or_else(|| run.caches.get(&in_slot))
            .ok_or_else(|| {
                GraphError::Internal(format!("node {name} input {in_port} has no frame"))
            })?;
        let mut collected: Vec<(u32, u32, Frame)> = Vec::new();
        for ty in 0..tile.grid.rows() {
            for tx in 0..tile.grid.cols() {
                if !tile.set.is_dirty(tx, ty) {
                    continue;
                }
                let (ox0, oy0, ox1, oy1) = tile.grid.tile_rect(tx, ty);
                let ix0 = ox0 / tile.scale;
                let ix1 = ox1.div_ceil(tile.scale);
                let iy0 = oy0 / tile.scale;
                let iy1 = oy1.div_ceil(tile.scale);
                let crop = crop_frame(in_frame, ix0, iy0, ix1, iy1).map_err(|error| {
                    GraphError::Internal(format!("node {name} tile crop failed: {error}"))
                })?;
                let mut tile_inputs = NodeInputs { frames: Vec::new() };
                tile_inputs.frames.push((in_port, Cow::Borrowed(&crop)));
                let outputs = node
                    .run(&tile_inputs)
                    .map_err(|source| GraphError::NodeFailed {
                        node: name.to_string(),
                        source,
                    })?;
                drop(tile_inputs);
                let produced = outputs.into_frames();
                let mut found: Option<Frame> = None;
                for (port, frame) in produced {
                    if port == out_port {
                        if found.is_some() {
                            return Err(GraphError::NodeOutputDuplicate {
                                node: name.to_string(),
                                port: port.to_string(),
                            });
                        }
                        found = Some(frame);
                    } else {
                        return Err(GraphError::NodeOutputUnknown {
                            node: name.to_string(),
                            port: port.to_string(),
                        });
                    }
                }
                let frame = found.ok_or_else(|| GraphError::NodeOutputMissing {
                    node: name.to_string(),
                    port: out_port.to_string(),
                })?;
                collected.push((tx, ty, frame));
            }
        }
        collected
    };
    // Phase 2: splice the tile outputs into the cached output
    // frame. No input borrow is live here.
    let cached = run
        .caches
        .get_mut(tile.out_slot)
        .ok_or_else(|| GraphError::Internal(format!("node {name} output has no cached frame")))?;
    for (tx, ty, tile_frame) in tile_outputs {
        blit_tile(name, cached, &tile_frame, tx, ty, tile.scale)?;
    }
    Ok(())
}

/// Crop a frame to the half-open pixel rectangle
/// `(x0, y0, x1, y1)`, tightly packed, keeping the input's
/// format, timestamp, and sequence. The rectangle must lie
/// inside the frame.
fn crop_frame(frame: &Frame, x0: u32, y0: u32, x1: u32, y1: u32) -> Result<Frame, FrameError> {
    if x1 < x0 || y1 < y0 || x1 > frame.width() || y1 > frame.height() {
        return Err(FrameError {
            message: format!(
                "crop ({x0},{y0})-({x1},{y1}) outside {}x{} frame",
                frame.width(),
                frame.height()
            ),
        });
    }
    let bpp = frame.bytes_per_pixel();
    let mut data = Vec::with_capacity((x1 - x0) as usize * (y1 - y0) as usize * bpp);
    for y in y0..y1 {
        data.extend_from_slice(&frame.row(y)[x0 as usize * bpp..x1 as usize * bpp]);
    }
    Frame::new(x1 - x0, y1 - y0, frame.format(), data).map(|cropped| {
        cropped
            .with_timestamp(frame.timestamp())
            .with_sequence(frame.sequence())
    })
}

/// Copy one output tile from a cropped run's output into the
/// cached output frame. The tile's output rectangle is
/// `(tx*16, ty*16)` to `(tx*16+16, ty*16+16)` clamped to the
/// cached frame; the cropped output starts at output pixel
/// `(ix0*scale, iy0*scale)`, where `(ix0, iy0)` is the input
/// footprint origin the caller cropped from, so the tile's
/// pixels sit at offset `(ox0 - ix0*scale, oy0 - iy0*scale)`
/// inside the cropped output.
fn blit_tile(
    name: &str,
    cached: &mut Frame,
    tile: &Frame,
    tx: u32,
    ty: u32,
    scale: u32,
) -> Result<(), GraphError> {
    if tile.format() != cached.format() {
        return Err(GraphError::Internal(format!(
            "node {name} tile output is {}, cached output is {}",
            tile.format(),
            cached.format()
        )));
    }
    let grid = TileGrid::from_dims(cached.width(), cached.height());
    let (ox0, oy0, ox1, oy1) = grid.tile_rect(tx, ty);
    if ox1 <= ox0 || oy1 <= oy0 {
        return Ok(());
    }
    let ix0 = ox0 / scale;
    let iy0 = oy0 / scale;
    let src_col = ox0 - ix0 * scale;
    let src_row_off = oy0 - iy0 * scale;
    let need_width = src_col + (ox1 - ox0);
    let need_height = src_row_off + (oy1 - oy0);
    if tile.width() < need_width || tile.height() < need_height {
        return Err(GraphError::Internal(format!(
            "node {name} tile output is {}x{}, smaller than the {}x{} tile it must cover",
            tile.width(),
            tile.height(),
            need_width,
            need_height
        )));
    }
    let bpp = cached.bytes_per_pixel();
    for y in oy0..oy1 {
        let src_row = tile.row(y - oy0 + src_row_off);
        let dst = &mut cached.row_mut(y)[ox0 as usize * bpp..ox1 as usize * bpp];
        let start = src_col as usize * bpp;
        let end = (src_col + (ox1 - ox0)) as usize * bpp;
        dst.copy_from_slice(&src_row[start..end]);
    }
    Ok(())
}
