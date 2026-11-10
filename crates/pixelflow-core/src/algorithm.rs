//! Algorithm contract: the stable interface every upscaling
//! algorithm implements, the registry entries describe, and the
//! CLI, the graph, and the processing crate all program against.
//!
//! A [`Processor`] is the execution half of the contract: one
//! frame in, one frame out. An [`AlgorithmFactory`] is the
//! instantiation half: it describes the algorithm once, validates
//! a configuration against that description, and builds
//! processors and graph nodes from it. The
//! [`AlgorithmDescriptor`] carries the description: stable id,
//! human-readable name and description, version, input and output
//! formats, supported scales, validated parameters, execution
//! backends, resource requirements, determinism, and the spatial
//! and temporal dependencies the graph executor reads. A
//! multi-stage algorithm declares its stages in the descriptor
//! and overrides [`AlgorithmFactory::create_nodes`] to build one
//! node per stage; nothing forces it into a single `process`
//! call.

use crate::format::PixelFormat;
use crate::graph::{Node, ProcessorNode, SpatialDependency, TemporalDependency};
use crate::pipeline::{Processor, StrideSupport};

/// Where an algorithm runs. The CPU side exists; the GPU side
/// is a declaration for now — a GPU worker defines the same
/// concept in its crate, and the coordinator reconciles the two
/// at integration. An algorithm lists every backend it can run
/// on; the registry refuses a backend it does not list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionBackend {
    /// The algorithm runs on the CPU, on [`Frame`] data.
    Cpu,
    /// The algorithm runs on the GPU, on GPU-backed resources.
    Gpu,
}

impl std::fmt::Display for ExecutionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionBackend::Cpu => write!(f, "cpu"),
            ExecutionBackend::Gpu => write!(f, "gpu"),
        }
    }
}

/// Whether an algorithm ships with the application or arrived
/// from outside it. The integration boundary is source-level — a
/// Rust crate interface — so an external algorithm is one whose
/// source lives outside this workspace and registers itself the
/// same way a built-in one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlgorithmOrigin {
    /// The algorithm ships in this workspace.
    BuiltIn,
    /// The algorithm arrived as outside source and registered
    /// itself against the same contract.
    External,
}

impl std::fmt::Display for AlgorithmOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlgorithmOrigin::BuiltIn => write!(f, "built-in"),
            AlgorithmOrigin::External => write!(f, "external"),
        }
    }
}

/// One validated configuration value, keyed by parameter name
/// in an [`AlgorithmConfig`].
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    /// An integer parameter value.
    Integer(i64),
    /// A floating-point parameter value.
    Float(f64),
    /// A boolean parameter value.
    Boolean(bool),
    /// A text parameter value.
    Text(String),
}

/// The accepted shape of one algorithm parameter: its type and
/// its range. Validation rejects a value outside the shape.
#[derive(Debug, Clone, PartialEq)]
pub enum ParameterKind {
    /// An integer in `min..=max`, inclusive.
    Integer {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
    },
    /// A float in `min..=max`, inclusive.
    Float {
        /// Smallest accepted value.
        min: f64,
        /// Largest accepted value.
        max: f64,
    },
    /// A boolean.
    Boolean,
    /// A text value, one of the listed choices.
    Choice(&'static [&'static str]),
}

/// One algorithm-specific parameter: its name, what it does,
/// its accepted shape, and its default.
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterDescriptor {
    /// Stable parameter name, used as the configuration key.
    pub name: &'static str,
    /// What the parameter controls.
    pub description: &'static str,
    /// Accepted type and range.
    pub kind: ParameterKind,
    /// Value used when the configuration omits the parameter.
    pub default: ConfigValue,
}

/// A validated set of algorithm-specific parameters: parameter
/// names to values. A configuration is validated against the
/// algorithm's [`ParameterDescriptor`] list before anything is
/// built from it; an unknown parameter, a wrong type, or a value
/// outside its range fails validation with
/// [`AlgorithmError::InvalidConfig`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlgorithmConfig {
    /// Parameter name and value, in insertion order.
    values: Vec<(String, ConfigValue)>,
}

impl AlgorithmConfig {
    /// An empty configuration: every parameter takes its
    /// default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `name` to `value`, replacing any earlier value for
    /// the same name.
    pub fn set(&mut self, name: &str, value: ConfigValue) -> &mut Self {
        if let Some(slot) = self.values.iter_mut().find(|(key, _)| key == name) {
            slot.1 = value;
        } else {
            self.values.push((name.to_string(), value));
        }
        self
    }

    /// The value for `name`, or `None` when it is not set.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ConfigValue> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// The integer value for `name`, or `None` when it is not
    /// set or is not an integer.
    #[must_use]
    pub fn integer(&self, name: &str) -> Option<i64> {
        match self.get(name) {
            Some(ConfigValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    /// The float value for `name`, or `None` when it is not set
    /// or is not a float.
    #[must_use]
    pub fn float(&self, name: &str) -> Option<f64> {
        match self.get(name) {
            Some(ConfigValue::Float(value)) => Some(*value),
            _ => None,
        }
    }

    /// The boolean value for `name`, or `None` when it is not
    /// set or is not a boolean.
    #[must_use]
    pub fn boolean(&self, name: &str) -> Option<bool> {
        match self.get(name) {
            Some(ConfigValue::Boolean(value)) => Some(*value),
            _ => None,
        }
    }

    /// The text value for `name`, or `None` when it is not set
    /// or is not text.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.get(name) {
            Some(ConfigValue::Text(value)) => Some(value.as_str()),
            _ => None,
        }
    }

    /// True when no parameter is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The set parameters, in insertion order.
    #[must_use]
    pub fn entries(&self) -> &[(String, ConfigValue)] {
        &self.values
    }
}

/// The full static description of one algorithm: everything a
/// caller needs to discover it, inspect it, validate a
/// configuration for it, and run it, without knowing its type.
/// Built from a [`Processor`] by
/// [`AlgorithmDescriptor::from_processor`], which reads the
/// processor's contract methods, so the description and the
/// implementation cannot drift apart.
#[derive(Debug, Clone, PartialEq)]
pub struct AlgorithmDescriptor {
    /// Stable id, e.g. "contour". The lookup key in the
    /// registry and on the command line.
    pub id: &'static str,
    /// Human-readable name, e.g. "Contour".
    pub display_name: &'static str,
    /// What the algorithm does, in one sentence.
    pub description: &'static str,
    /// Algorithm version.
    pub version: &'static str,
    /// Input pixel formats the algorithm accepts.
    pub input_formats: Vec<PixelFormat>,
    /// Output pixel formats the algorithm produces.
    pub output_formats: Vec<PixelFormat>,
    /// Integer scale factors the algorithm supports.
    pub supported_scales: Vec<u32>,
    /// Execution backends the algorithm can run on.
    pub backends: Vec<ExecutionBackend>,
    /// Whether the algorithm ships with the application.
    pub origin: AlgorithmOrigin,
    /// What input area one output pixel depends on.
    pub spatial_dependency: SpatialDependency,
    /// What history the algorithm keeps between frames.
    pub temporal_dependency: TemporalDependency,
    /// What row layouts the algorithm accepts.
    pub stride_support: StrideSupport,
    /// Whether a cropped run produces exactly the
    /// corresponding crop of the whole-frame output.
    pub tile_safe: bool,
    /// Validated algorithm-specific parameters.
    pub parameters: Vec<ParameterDescriptor>,
    /// Working memory per output pixel, in bytes, when the
    /// algorithm's working set is a flat per-pixel figure.
    /// `None` when the working set has no such figure — the
    /// algorithm's own budget check states its model instead.
    pub working_bytes_per_output_pixel: Option<u64>,
    /// Intermediate resources a multi-stage algorithm holds
    /// between its stages, named so a caller can account for
    /// them. Empty for a single-stage algorithm.
    pub intermediate_resources: Vec<&'static str>,
    /// True when the same input frame, scale, and
    /// configuration always produce byte-identical output.
    pub deterministic: bool,
    /// What determinism means for this algorithm, or why it
    /// does not hold.
    pub determinism_note: &'static str,
    /// Graph node stages, in run order: one name per node
    /// [`AlgorithmFactory::create_nodes`] builds. A
    /// single-stage algorithm lists its own id here.
    pub stages: Vec<&'static str>,
}

impl AlgorithmDescriptor {
    /// Build the descriptor from a processor, reading every
    /// field from the processor's contract methods. The
    /// `origin` says whether the algorithm ships with the
    /// application.
    #[must_use]
    pub fn from_processor(processor: &dyn Processor, origin: AlgorithmOrigin) -> Self {
        let stages = processor.stage_names();
        let stages = if stages.is_empty() {
            vec![processor.algorithm_id()]
        } else {
            stages.to_vec()
        };
        Self {
            id: processor.algorithm_id(),
            display_name: processor.algorithm_id(),
            description: processor.description(),
            version: processor.version(),
            input_formats: processor.input_formats().to_vec(),
            output_formats: processor.output_formats().to_vec(),
            supported_scales: processor.supported_scales().to_vec(),
            backends: processor.backends().to_vec(),
            origin,
            spatial_dependency: processor.spatial_dependency(),
            temporal_dependency: processor.temporal_dependency(),
            stride_support: processor.stride_support(),
            tile_safe: processor.tile_safe(),
            parameters: processor.parameters().to_vec(),
            working_bytes_per_output_pixel: processor.working_bytes_per_output_pixel(),
            intermediate_resources: processor.intermediate_resources().to_vec(),
            deterministic: processor.deterministic(),
            determinism_note: processor.determinism_note(),
            stages,
        }
    }

    /// True when the algorithm supports `scale`.
    #[must_use]
    pub fn supports_scale(&self, scale: u32) -> bool {
        self.supported_scales.contains(&scale)
    }

    /// True when the algorithm accepts `format` as input.
    #[must_use]
    pub fn supports_input_format(&self, format: PixelFormat) -> bool {
        self.input_formats.contains(&format)
    }

    /// Output dimensions for an input frame of `width` by
    /// `height` at `scale`: both dimensions times the scale.
    /// `None` when the scale is unsupported or the dimensions
    /// overflow.
    #[must_use]
    pub fn output_dimensions(&self, width: u32, height: u32, scale: u32) -> Option<(u32, u32)> {
        if !self.supports_scale(scale) {
            return None;
        }
        let out_width = width.checked_mul(scale)?;
        let out_height = height.checked_mul(scale)?;
        Some((out_width, out_height))
    }

    /// Estimated working memory for `output_pixels` output
    /// pixels, from the flat per-pixel figure, or `None` when
    /// the algorithm states no such figure. The estimate is a
    /// caller-side budget aid, not a refusal: the algorithm's
    /// own check enforces a configured budget.
    #[must_use]
    pub fn estimated_working_bytes(&self, output_pixels: u64) -> Option<u64> {
        self.working_bytes_per_output_pixel
            .and_then(|per_pixel| output_pixels.checked_mul(per_pixel))
    }
}

/// What can go wrong while discovering, validating, or
/// instantiating an algorithm. Execution failures keep their own
/// type, the process error; these errors happen before any frame
/// is processed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlgorithmError {
    /// No algorithm with this id is registered. The available
    /// ids are listed so the caller can report them.
    UnknownAlgorithm {
        /// The id that was looked up.
        id: String,
        /// Ids the registry does hold.
        available: Vec<String>,
    },
    /// The scale factor is not one the algorithm supports.
    UnsupportedScale {
        /// Id of the algorithm that rejected the scale.
        algorithm: &'static str,
        /// The scale factor that was requested.
        scale: u32,
        /// Scale factors the algorithm supports.
        supported: Vec<u32>,
    },
    /// The algorithm cannot run on this execution backend.
    UnsupportedBackend {
        /// Id of the algorithm that rejected the backend.
        algorithm: &'static str,
        /// The backend that was requested.
        backend: ExecutionBackend,
    },
    /// A configuration value is unknown, wrongly typed, or
    /// outside its parameter's range.
    InvalidConfig {
        /// Id of the algorithm the configuration was for.
        algorithm: &'static str,
        /// Parameter name that failed validation.
        parameter: String,
        /// Why the value was rejected.
        message: String,
    },
    /// The graph built from the algorithm's nodes failed to
    /// validate; the message is the graph error.
    GraphBuild(String),
}

impl std::fmt::Display for AlgorithmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlgorithmError::UnknownAlgorithm { id, available } => {
                write!(
                    f,
                    "unknown algorithm {id:?}; available: {}",
                    available.join(", ")
                )
            }
            AlgorithmError::UnsupportedScale {
                algorithm,
                scale,
                supported,
            } => {
                let scales: Vec<String> =
                    supported.iter().map(|scale| format!("{scale}x")).collect();
                write!(
                    f,
                    "scale {scale} is not supported by {algorithm}; supported scales: {}",
                    scales.join(", ")
                )
            }
            AlgorithmError::UnsupportedBackend { algorithm, backend } => {
                write!(f, "{algorithm} cannot run on the {backend} backend")
            }
            AlgorithmError::InvalidConfig {
                algorithm,
                parameter,
                message,
            } => {
                write!(
                    f,
                    "{algorithm}: invalid value for parameter {parameter}: {message}"
                )
            }
            AlgorithmError::GraphBuild(message) => {
                write!(f, "algorithm graph failed to build: {message}")
            }
        }
    }
}

impl std::error::Error for AlgorithmError {}

/// Validate `config` against `parameters`: every set parameter
/// must be declared, of the declared type, and inside its
/// range. Parameters the configuration omits take their
/// defaults and need no entry.
///
/// # Errors
///
/// Returns [`AlgorithmError::InvalidConfig`] for an unknown
/// parameter, a wrongly typed value, or a value outside its
/// range.
pub fn validate_config(
    parameters: &[ParameterDescriptor],
    algorithm: &'static str,
    config: &AlgorithmConfig,
) -> Result<(), AlgorithmError> {
    for (name, value) in config.entries() {
        let parameter = parameters.iter().find(|candidate| candidate.name == name);
        let Some(parameter) = parameter else {
            return Err(AlgorithmError::InvalidConfig {
                algorithm,
                parameter: name.clone(),
                message: "unknown parameter".to_string(),
            });
        };
        check_value(algorithm, parameter, value)?;
    }
    Ok(())
}

/// Check one configuration value against its parameter's kind
/// and range.
fn check_value(
    algorithm: &'static str,
    parameter: &ParameterDescriptor,
    value: &ConfigValue,
) -> Result<(), AlgorithmError> {
    let invalid = |message: &str| AlgorithmError::InvalidConfig {
        algorithm,
        parameter: parameter.name.to_string(),
        message: message.to_string(),
    };
    match (&parameter.kind, value) {
        (ParameterKind::Integer { min, max }, ConfigValue::Integer(value)) => {
            if value < min || value > max {
                return Err(invalid(&format!(
                    "integer {value} is outside {min}..={max}"
                )));
            }
        }
        (ParameterKind::Float { min, max }, ConfigValue::Float(value)) => {
            if value < min || value > max {
                return Err(invalid(&format!("float {value} is outside {min}..={max}")));
            }
        }
        (ParameterKind::Boolean, ConfigValue::Boolean(_)) => {}
        (ParameterKind::Choice(choices), ConfigValue::Text(value)) => {
            if !choices.contains(&value.as_str()) {
                return Err(invalid(&format!(
                    "value {value:?} is not one of {}",
                    choices.join(", ")
                )));
            }
        }
        (_, _) => {
            return Err(invalid("value has the wrong type for this parameter"));
        }
    }
    Ok(())
}

/// The instantiation half of the algorithm contract: one value
/// per algorithm, describing it, validating configurations for
/// it, and building processors and graph nodes from it.
/// Registering an algorithm means implementing this trait and
/// handing one value to the registry; nothing else is needed,
/// and no other crate changes when a new algorithm arrives.
pub trait AlgorithmFactory {
    /// The algorithm's static description.
    fn descriptor(&self) -> AlgorithmDescriptor;

    /// Build a processor from a validated configuration.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::InvalidConfig`] when the
    /// configuration fails validation.
    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError>;

    /// Build the graph nodes for the algorithm at `scale`, in
    /// run order, one node per name in the descriptor's
    /// `stages`. The default builds one [`ProcessorNode`] from
    /// the processor [`AlgorithmFactory::create`] returns, using
    /// the spatial dependency the processor declares. A
    /// multi-stage algorithm overrides this to build one node
    /// per stage instead of forcing its stages into a single
    /// `process` call.
    ///
    /// # Errors
    ///
    /// Returns [`AlgorithmError::UnsupportedScale`] when the
    /// scale is not supported, or
    /// [`AlgorithmError::InvalidConfig`] when the configuration
    /// fails validation.
    fn create_nodes(
        &self,
        config: &AlgorithmConfig,
        scale: u32,
    ) -> Result<Vec<Box<dyn Node>>, AlgorithmError> {
        let descriptor = self.descriptor();
        if !descriptor.supports_scale(scale) {
            return Err(AlgorithmError::UnsupportedScale {
                algorithm: descriptor.id,
                scale,
                supported: descriptor.supported_scales.clone(),
            });
        }
        let processor = self.create(config)?;
        Ok(vec![Box::new(ProcessorNode::from_processor(
            processor, scale,
        ))])
    }
}
