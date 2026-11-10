//! Contour and Nearest as graph nodes: their dependency
//! declarations, a chain and a branch through the graph, and
//! proof the graph adds nothing to the output — the graph's
//! result equals the direct `Processor` result byte for byte.

use pixelflow_core::{
    Frame, GraphBuilder, Node, PixelFormat, ProcessError, Processor, Source, SpatialDependency,
    StrideSupport,
};
use pixelflow_processing::{Contour, Nearest};

/// A small frame with a diagonal edge: dark top-left, bright
/// bottom-right, and a mid-tone band between them.
fn diagonal_frame() -> Result<Frame, String> {
    let mut data = Vec::with_capacity(4 * 4 * 3);
    for y in 0..4 {
        for x in 0..4 {
            let v: u8 = if x + y < 3 {
                20
            } else if x + y < 5 {
                128
            } else {
                240
            };
            data.extend_from_slice(&[v, v, v]);
        }
    }
    Frame::new(4, 4, PixelFormat::Rgb8, data).map_err(|e| e.to_string())
}

#[test]
fn contour_declares_global_and_nearest_declares_local() {
    let contour = Contour::node(2);
    assert_eq!(contour.name(), "contour");
    assert_eq!(contour.input_ports(), &["in"]);
    assert_eq!(contour.output_ports(), &["out"]);
    assert_eq!(contour.spatial_dependency(), SpatialDependency::Global);
    assert_eq!(contour.stride_support(), StrideSupport::TightOnly);
    let nearest = Nearest::node(2);
    assert_eq!(nearest.name(), "nearest");
    assert_eq!(nearest.spatial_dependency(), SpatialDependency::Local);
    assert_eq!(nearest.stride_support(), StrideSupport::Any);
}

#[test]
fn chain_matches_direct_processor_output() -> Result<(), String> {
    let frame = diagonal_frame()?.with_sequence(5);
    let mut builder = GraphBuilder::new();
    let mut graph = std::mem::take(
        builder
            .add_input("in", PixelFormat::Rgb8)
            .add_node("nearest", Box::new(Nearest::node(2)))
            .add_node("contour", Box::new(Contour::node(2)))
            .connect(Source::input("in"), "nearest", "in")
            .connect(Source::node("nearest", "out"), "contour", "in")
            .add_output("out", "contour", "out"),
    )
    .build()
    .map_err(|e| e.to_string())?;
    let outputs = graph
        .run(vec![("in", frame.clone())])
        .map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].1.sequence(), 5);
    // Direct computation through the Processor trait.
    let direct = Contour
        .process(&Nearest.process(&frame, 2).map_err(|e| e.to_string())?, 2)
        .map_err(|e| e.to_string())?;
    assert_eq!(outputs[0].1.data(), direct.data());
    assert_eq!((outputs[0].1.width(), outputs[0].1.height()), (16, 16));
    Ok(())
}

#[test]
fn branch_matches_direct_outputs() -> Result<(), String> {
    let frame = diagonal_frame()?;
    let mut builder = GraphBuilder::new();
    let mut graph = std::mem::take(
        builder
            .add_input("in", PixelFormat::Rgb8)
            .add_node("contour", Box::new(Contour::node(2)))
            .add_node("nearest", Box::new(Nearest::node(2)))
            .connect(Source::input("in"), "contour", "in")
            .connect(Source::input("in"), "nearest", "in")
            .add_output("smooth", "contour", "out")
            .add_output("blocky", "nearest", "out"),
    )
    .build()
    .map_err(|e| e.to_string())?;
    let outputs = graph
        .run(vec![("in", frame.clone())])
        .map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 2);
    let mut smooth: Option<Frame> = None;
    let mut blocky: Option<Frame> = None;
    for (name, out) in outputs {
        match name.as_str() {
            "smooth" => smooth = Some(out),
            "blocky" => blocky = Some(out),
            _ => return Err(format!("unexpected output {name}")),
        }
    }
    let smooth = smooth.ok_or_else(|| "missing smooth output".to_string())?;
    let blocky = blocky.ok_or_else(|| "missing blocky output".to_string())?;
    let direct_contour = Contour.process(&frame, 2).map_err(|e| e.to_string())?;
    let direct_nearest = Nearest.process(&frame, 2).map_err(|e| e.to_string())?;
    assert_eq!(smooth.data(), direct_contour.data());
    assert_eq!(blocky.data(), direct_nearest.data());
    Ok(())
}

#[test]
fn strided_input_through_contour_node_matches_tight() -> Result<(), String> {
    let tight = diagonal_frame()?;
    // Same pixels, 4 bytes of padding per row (stride 16).
    let mut padded = Vec::with_capacity(4 * 16);
    for y in 0..4 {
        padded.extend_from_slice(&tight.row(y));
        padded.extend_from_slice(&[0, 0, 0, 0]);
    }
    let strided =
        Frame::new_with_stride(4, 4, PixelFormat::Rgb8, 16, padded).map_err(|e| e.to_string())?;
    let run = |input: Frame| -> Result<Frame, String> {
        let mut builder = GraphBuilder::new();
        let mut graph = std::mem::take(
            builder
                .add_input("in", PixelFormat::Rgb8)
                .add_node("contour", Box::new(Contour::node(2)))
                .connect(Source::input("in"), "contour", "in")
                .add_output("out", "contour", "out"),
        )
        .build()
        .map_err(|e| e.to_string())?;
        let mut outputs = graph.run(vec![("in", input)]).map_err(|e| e.to_string())?;
        outputs
            .pop()
            .map(|(_, frame)| frame)
            .ok_or_else(|| "graph produced no output".to_string())
    };
    let from_tight = run(tight)?;
    let from_strided = run(strided)?;
    assert_eq!(from_strided.data(), from_tight.data());
    Ok(())
}

#[test]
fn unsupported_scale_fails_through_the_graph() -> Result<(), String> {
    let frame = diagonal_frame()?;
    let mut builder = GraphBuilder::new();
    let mut graph = std::mem::take(
        builder
            .add_input("in", PixelFormat::Rgb8)
            .add_node("contour", Box::new(Contour::node(3)))
            .connect(Source::input("in"), "contour", "in")
            .add_output("out", "contour", "out"),
    )
    .build()
    .map_err(|e| e.to_string())?;
    let result = graph.run(vec![("in", frame)]);
    assert!(matches!(
        result,
        Err(pixelflow_core::GraphError::NodeFailed { node, source })
            if node == "contour"
                && source
                    == ProcessError::UnsupportedScale {
                        algorithm: "contour",
                        scale: 3,
                    }
    ));
    Ok(())
}

#[test]
fn limited_contour_node_declares_global_and_matches_direct_output() -> Result<(), String> {
    let frame = diagonal_frame()?;
    let node = Contour::with_memory_budget(u64::MAX).node(2);
    assert_eq!(node.name(), "contour");
    assert_eq!(node.spatial_dependency(), SpatialDependency::Global);
    let mut builder = GraphBuilder::new();
    let mut graph = std::mem::take(
        builder
            .add_input("in", PixelFormat::Rgb8)
            .add_node("contour", Box::new(node))
            .connect(Source::input("in"), "contour", "in")
            .add_output("out", "contour", "out"),
    )
    .build()
    .map_err(|e| e.to_string())?;
    let outputs = graph
        .run(vec![("in", frame.clone())])
        .map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 1);
    let direct = Contour.process(&frame, 2).map_err(|e| e.to_string())?;
    assert_eq!(outputs[0].1.data(), direct.data());
    Ok(())
}
