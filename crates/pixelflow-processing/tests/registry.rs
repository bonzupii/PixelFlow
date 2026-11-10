//! Registry tests: discovery, inspection, validation,
//! instantiation, and graph construction through the registry,
//! with no per-algorithm match arm. The last test registers a
//! new two-stage algorithm from this file alone and runs it
//! through the registry graph, which is the proof that adding
//! an algorithm needs no change to capture, presentation, or
//! CLI code.

use pixelflow_core::{
    AlgorithmConfig, AlgorithmError, AlgorithmFactory, AlgorithmOrigin, ConfigValue,
    ExecutionBackend, Frame, Graph, Node, PixelFormat, ProcessError, Processor, SpatialDependency,
};
use pixelflow_processing::AlgorithmRegistry;
use pixelflow_processing::algorithms::nearest::Nearest;

/// A 2x2 RGB8 frame with distinct pixels.
fn frame_2x2() -> Result<Frame, String> {
    Frame::new(
        2,
        2,
        PixelFormat::Rgb8,
        vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120],
    )
    .map_err(|e| e.to_string())
}

#[test]
fn builtin_registry_lists_five_algorithms() {
    let registry = AlgorithmRegistry::builtin();
    assert_eq!(
        registry.ids(),
        vec!["contour", "nearest", "bilinear", "bicubic", "pixelflow"]
    );
    assert_eq!(registry.descriptors().len(), 5);
}

#[test]
fn unknown_algorithm_error_lists_the_registered_ids() {
    let registry = AlgorithmRegistry::builtin();
    let error = registry
        .descriptor("nope")
        .expect_err("expected an unknown-algorithm error");
    assert_eq!(
        error,
        AlgorithmError::UnknownAlgorithm {
            id: "nope".to_string(),
            available: vec![
                "contour".to_string(),
                "nearest".to_string(),
                "bilinear".to_string(),
                "bicubic".to_string(),
                "pixelflow".to_string(),
            ],
        }
    );
    assert!(
        error
            .to_string()
            .contains("available: contour, nearest, bilinear, bicubic, pixelflow")
    );
}

#[test]
fn contour_descriptor_states_its_contract() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let descriptor = registry.descriptor("contour").map_err(|e| e.to_string())?;
    assert_eq!(descriptor.display_name, "Contour");
    assert_eq!(
        descriptor.description,
        "gated merge, adaptive labelset, edge follower"
    );
    assert!(!descriptor.description.contains("algorithm 20"));
    assert_eq!(descriptor.input_formats, vec![PixelFormat::Rgb8]);
    assert_eq!(descriptor.output_formats, vec![PixelFormat::Rgb8]);
    assert_eq!(descriptor.supported_scales, vec![2, 4]);
    assert_eq!(descriptor.backends, vec![ExecutionBackend::Cpu]);
    assert_eq!(descriptor.origin, AlgorithmOrigin::BuiltIn);
    assert_eq!(descriptor.spatial_dependency, SpatialDependency::Global);
    assert!(!descriptor.tile_safe);
    assert_eq!(descriptor.stages, vec!["contour"]);
    assert!(descriptor.deterministic);
    assert_eq!(descriptor.output_dimensions(4, 4, 2), Some((8, 8)));
    assert_eq!(descriptor.output_dimensions(4, 4, 3), None);
    assert_eq!(
        descriptor.parameters.len(),
        1,
        "contour declares its memory-budget parameter"
    );
    assert_eq!(descriptor.parameters[0].name, "memory_budget_bytes");
    Ok(())
}

#[test]
fn nearest_descriptor_is_local_and_tile_safe() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let descriptor = registry.descriptor("nearest").map_err(|e| e.to_string())?;
    assert_eq!(descriptor.spatial_dependency, SpatialDependency::Local);
    assert!(descriptor.tile_safe);
    assert!(descriptor.parameters.is_empty());
    assert_eq!(descriptor.working_bytes_per_output_pixel, Some(3));
    assert_eq!(descriptor.estimated_working_bytes(100), Some(300));
    Ok(())
}

#[test]
fn bilinear_descriptor_declares_a_radius_1_neighborhood() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let descriptor = registry.descriptor("bilinear").map_err(|e| e.to_string())?;
    assert_eq!(
        descriptor.spatial_dependency,
        SpatialDependency::Neighborhood { radius: 1 }
    );
    assert!(!descriptor.tile_safe);
    assert_eq!(descriptor.supported_scales, vec![2, 4]);
    Ok(())
}

#[test]
fn bicubic_descriptor_declares_a_radius_2_neighborhood() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let descriptor = registry.descriptor("bicubic").map_err(|e| e.to_string())?;
    assert_eq!(descriptor.display_name, "Bicubic");
    assert_eq!(
        descriptor.spatial_dependency,
        SpatialDependency::Neighborhood { radius: 2 }
    );
    assert!(!descriptor.tile_safe);
    assert_eq!(descriptor.supported_scales, vec![2, 4]);
    assert_eq!(descriptor.working_bytes_per_output_pixel, Some(5));
    assert_eq!(descriptor.estimated_working_bytes(100), Some(500));
    Ok(())
}

#[test]
fn every_registered_algorithm_processes_through_the_registry() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let frame = frame_2x2()?;
    let config = AlgorithmConfig::new();
    for id in registry.ids() {
        let processor = registry
            .create_processor(id, &config)
            .map_err(|e| e.to_string())?;
        let out = processor.process(&frame, 2).map_err(|e| e.to_string())?;
        assert_eq!((out.width(), out.height()), (4, 4));
        assert_eq!(out.format(), PixelFormat::Rgb8);
    }
    Ok(())
}

#[test]
fn every_registered_algorithm_runs_through_its_registry_graph() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let frame = frame_2x2()?;
    let config = AlgorithmConfig::new();
    for id in registry.ids() {
        let direct = registry
            .create_processor(id, &config)
            .map_err(|e| e.to_string())?
            .process(&frame, 2)
            .map_err(|e| e.to_string())?;
        let mut graph = registry
            .build_graph(id, &config, 2)
            .map_err(|e| e.to_string())?;
        let outputs = graph
            .run(vec![("in", frame.clone())])
            .map_err(|e| e.to_string())?;
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            outputs[0].1.data(),
            direct.data(),
            "graph output of {id} differs"
        );
    }
    Ok(())
}

#[test]
fn config_validation_rejects_unknown_keys_and_bad_values() {
    let registry = AlgorithmRegistry::builtin();
    let mut unknown = AlgorithmConfig::new();
    unknown.set("nope", ConfigValue::Integer(1));
    assert!(matches!(
        registry.create_processor("nearest", &unknown),
        Err(AlgorithmError::InvalidConfig { .. })
    ));
    let mut wrong_type = AlgorithmConfig::new();
    wrong_type.set("memory_budget_bytes", ConfigValue::Text("1".to_string()));
    assert!(matches!(
        registry.create_processor("contour", &wrong_type),
        Err(AlgorithmError::InvalidConfig { .. })
    ));
    let mut negative = AlgorithmConfig::new();
    negative.set("memory_budget_bytes", ConfigValue::Integer(-1));
    assert!(matches!(
        registry.create_processor("contour", &negative),
        Err(AlgorithmError::InvalidConfig { .. })
    ));
}

#[test]
fn contour_budget_from_config_is_enforced() -> Result<(), String> {
    let registry = AlgorithmRegistry::builtin();
    let frame = frame_2x2()?;
    let mut config = AlgorithmConfig::new();
    config.set("memory_budget_bytes", ConfigValue::Integer(1));
    let processor = registry
        .create_processor("contour", &config)
        .map_err(|e| e.to_string())?;
    assert_eq!(processor.name(), "contour");
    assert!(matches!(
        processor.process(&frame, 2),
        Err(ProcessError::MemoryBudgetExceeded { .. })
    ));
    Ok(())
}

#[test]
fn registry_rejects_unsupported_scales_before_processing() {
    let registry = AlgorithmRegistry::builtin();
    let config = AlgorithmConfig::new();
    assert!(
        matches!(
            registry.create_nodes("nearest", &config, 3),
            Err(AlgorithmError::UnsupportedScale {
                algorithm: "nearest",
                scale: 3,
                supported,
            }) if supported == vec![2, 4]
        ),
        "expected an unsupported-scale error naming scale 3 and [2, 4]"
    );
}

/// An inverter: same-size frame, every channel subtracted
/// from 255. Scale 1 only; the second stage of the
/// two-stage test algorithm below.
struct Invert;

impl Processor for Invert {
    fn name(&self) -> &'static str {
        "invert"
    }

    fn supported_scales(&self) -> &[u32] {
        &[1]
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if scale != 1 {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "invert",
                scale,
            });
        }
        let data: Vec<u8> = frame.data().iter().map(|byte| 255 - byte).collect();
        Frame::new(frame.width(), frame.height(), frame.format(), data)
            .map_err(|e| ProcessError::Internal(e.to_string()))
    }
}

/// A two-stage test algorithm: nearest-neighbour 2x, then
/// invert. Registered here, in the test alone — no change to
/// any application crate — to prove the registry accepts a new
/// algorithm by contract implementation plus one registration
/// line.
struct DoubleInvert;

impl AlgorithmFactory for DoubleInvert {
    fn descriptor(&self) -> pixelflow_core::AlgorithmDescriptor {
        let mut descriptor = pixelflow_core::AlgorithmDescriptor::from_processor(
            &Nearest,
            AlgorithmOrigin::External,
        );
        descriptor.id = "double-invert";
        descriptor.display_name = "DoubleInvert";
        descriptor.description = "nearest-neighbour 2x, then invert";
        descriptor.supported_scales = vec![2];
        descriptor.stages = vec!["double", "invert"];
        descriptor.intermediate_resources = vec!["2x nearest-neighbour frame"];
        descriptor
    }

    fn create(&self, config: &AlgorithmConfig) -> Result<Box<dyn Processor>, AlgorithmError> {
        Nearest.validate_config(config)?;
        Ok(Box::new(Nearest))
    }

    fn create_nodes(
        &self,
        config: &AlgorithmConfig,
        scale: u32,
    ) -> Result<Vec<Box<dyn Node>>, AlgorithmError> {
        if scale != 2 {
            return Err(AlgorithmError::UnsupportedScale {
                algorithm: "double-invert",
                scale,
                supported: vec![2],
            });
        }
        self.create(config)?;
        Ok(vec![
            Box::new(Nearest::node(2)),
            Box::new(pixelflow_core::ProcessorNode::new(
                Invert,
                1,
                SpatialDependency::Local,
            )),
        ])
    }
}

#[test]
fn a_new_two_stage_algorithm_registers_and_runs() -> Result<(), String> {
    let mut registry = AlgorithmRegistry::builtin();
    registry.register(Box::new(DoubleInvert));
    assert!(registry.ids().contains(&"double-invert"));
    let descriptor = registry
        .descriptor("double-invert")
        .map_err(|e| e.to_string())?;
    assert_eq!(descriptor.stages, vec!["double", "invert"]);
    assert_eq!(descriptor.origin, AlgorithmOrigin::External);
    let frame = Frame::new(1, 1, PixelFormat::Rgb8, vec![10, 20, 30]).map_err(|e| e.to_string())?;
    let mut graph: Graph = registry
        .build_graph("double-invert", &AlgorithmConfig::new(), 2)
        .map_err(|e| e.to_string())?;
    let outputs = graph.run(vec![("in", frame)]).map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 1);
    assert_eq!((outputs[0].1.width(), outputs[0].1.height()), (2, 2));
    assert_eq!(outputs[0].1.data(), vec![245, 235, 225].repeat(4));
    Ok(())
}

#[test]
fn registering_an_id_twice_replaces_it() {
    let mut registry = AlgorithmRegistry::builtin();
    let before = registry.ids().len();
    registry.register(Box::new(pixelflow_processing::BilinearAlgorithm));
    assert_eq!(registry.ids().len(), before);
    assert_eq!(
        registry
            .ids()
            .iter()
            .filter(|id| **id == "bilinear")
            .count(),
        1
    );
}
