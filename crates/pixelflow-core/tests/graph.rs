//! Processing graph tests: chains, branches, two-input nodes,
//! validation failures, and error propagation. The test
//! processors and nodes live here; the only real algorithms in
//! this file are the ones wrapped by `ProcessorNode`, and they
//! are test doubles, not product filters.

use pixelflow_core::{
    Frame, Graph, GraphBuilder, GraphError, Node, NodeInputs, NodeOutputs, PixelFormat,
    ProcessError, Processor, ProcessorNode, Source, SpatialDependency, StrideSupport,
    TemporalDependency,
};

/// Build a tight RGB8 frame for a test.
fn rgb_frame(width: u32, height: u32, data: Vec<u8>) -> Result<Frame, String> {
    Frame::new(width, height, PixelFormat::Rgb8, data).map_err(|e| e.to_string())
}

/// Test processor: nearest-neighbour 2x, stride-aware, Local.
struct Doubler;

impl Processor for Doubler {
    fn name(&self) -> &'static str {
        "doubler"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2]
    }

    /// The double is nearest-neighbour 2x: each output pixel
    /// copies one input pixel, so a cropped run produces exactly
    /// the corresponding crop of the whole-frame output. This is
    /// what the incremental executor's tile path runs on.
    fn tile_safe(&self) -> bool {
        true
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if scale != 2 {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "doubler",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "doubler",
                format: frame.format().to_string(),
            });
        }
        let s = 2usize;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let mut data = vec![0u8; w * s * h * s * 3];
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                let src = frame.pixel(x, y);
                for dy in 0..s {
                    for dx in 0..s {
                        let dst = ((y as usize * s + dy) * w * s + (x as usize * s + dx)) * 3;
                        data[dst..dst + 3].copy_from_slice(src);
                    }
                }
            }
        }
        Frame::new(
            frame.width().checked_mul(scale).ok_or_else(|| {
                ProcessError::Internal("doubler output width exceeds u32 range".to_string())
            })?,
            frame.height().checked_mul(scale).ok_or_else(|| {
                ProcessError::Internal("doubler output height exceeds u32 range".to_string())
            })?,
            PixelFormat::Rgb8,
            data,
        )
        .map_err(|e| ProcessError::Internal(e.to_string()))
        .map(|out| {
            out.with_timestamp(frame.timestamp())
                .with_sequence(frame.sequence())
        })
    }
}

/// Test processor: nearest-neighbour 2x that reads its input
/// buffer flat, so it only accepts tight frames. The executor
/// must pack strided input before running it.
struct TightDoubler;

impl Processor for TightDoubler {
    fn name(&self) -> &'static str {
        "tight-doubler"
    }

    fn supported_scales(&self) -> &[u32] {
        &[2]
    }

    fn stride_support(&self) -> StrideSupport {
        StrideSupport::TightOnly
    }

    fn process(&self, frame: &Frame, scale: u32) -> Result<Frame, ProcessError> {
        if scale != 2 {
            return Err(ProcessError::UnsupportedScale {
                algorithm: "tight-doubler",
                scale,
            });
        }
        if frame.format() != PixelFormat::Rgb8 {
            return Err(ProcessError::UnsupportedFormat {
                algorithm: "tight-doubler",
                format: frame.format().to_string(),
            });
        }
        // Reads the buffer flat: correct only on a tight frame.
        // The graph executor packs before this node runs.
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let mut data = vec![0u8; w * 2 * h * 2 * 3];
        for y in 0..h {
            for x in 0..w {
                let src = &frame.data()[(y * w + x) * 3..(y * w + x) * 3 + 3];
                for dy in 0..2 {
                    for dx in 0..2 {
                        let dst = ((y * 2 + dy) * w * 2 + (x * 2 + dx)) * 3;
                        data[dst..dst + 3].copy_from_slice(src);
                    }
                }
            }
        }
        Frame::new(
            frame.width().checked_mul(scale).ok_or_else(|| {
                ProcessError::Internal("tight-doubler output width exceeds u32 range".to_string())
            })?,
            frame.height().checked_mul(scale).ok_or_else(|| {
                ProcessError::Internal("tight-doubler output height exceeds u32 range".to_string())
            })?,
            PixelFormat::Rgb8,
            data,
        )
        .map_err(|e| ProcessError::Internal(e.to_string()))
        .map(|out| {
            out.with_timestamp(frame.timestamp())
                .with_sequence(frame.sequence())
        })
    }
}

/// Test processor: returns its input unchanged, any scale.
struct Identity;

impl Processor for Identity {
    fn name(&self) -> &'static str {
        "identity"
    }

    fn supported_scales(&self) -> &[u32] {
        &[1, 2, 4]
    }

    fn process(&self, frame: &Frame, _scale: u32) -> Result<Frame, ProcessError> {
        Ok(frame.clone())
    }
}

/// Test node: averages its two RGB8 inputs channel by channel.
/// Both inputs must be tight frames of the same size.
struct BlendNode;

impl Node for BlendNode {
    fn name(&self) -> &'static str {
        "blend"
    }

    fn input_ports(&self) -> &[&'static str] {
        &["a", "b"]
    }

    fn output_ports(&self) -> &[&'static str] {
        &["out"]
    }

    fn input_format(&self, port: &str) -> Option<PixelFormat> {
        if port == "a" || port == "b" {
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
        SpatialDependency::Local
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    fn run(&mut self, inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError> {
        let a = inputs
            .get("a")
            .ok_or_else(|| ProcessError::Internal("blend ran without input a".to_string()))?;
        let b = inputs
            .get("b")
            .ok_or_else(|| ProcessError::Internal("blend ran without input b".to_string()))?;
        if a.width() != b.width() || a.height() != b.height() {
            return Err(ProcessError::Internal(
                "blend inputs differ in size".to_string(),
            ));
        }
        // Integer average without a widening cast: (x + y) / 2
        // equals x/2 + y/2 + ((x % 2) + (y % 2)) / 2, all in u8.
        let mut data = Vec::with_capacity(a.data().len());
        for (x, y) in a.data().iter().zip(b.data().iter()) {
            data.push(x / 2 + y / 2 + (x % 2 + y % 2) / 2);
        }
        let out = Frame::new(a.width(), a.height(), PixelFormat::Rgb8, data)
            .map_err(|e| ProcessError::Internal(e.to_string()))?
            .with_timestamp(a.timestamp())
            .with_sequence(a.sequence());
        let mut outputs = NodeOutputs::new();
        outputs.add("out", out);
        Ok(outputs)
    }
}

/// Test node: always fails.
struct FailNode;

impl Node for FailNode {
    fn name(&self) -> &'static str {
        "fail"
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
        SpatialDependency::Local
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    fn run(&mut self, _inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError> {
        Err(ProcessError::Internal("fail node always fails".to_string()))
    }
}

/// Test node: converts RGB8 to Gray8 by taking the red channel.
struct GrayNode;

impl Node for GrayNode {
    fn name(&self) -> &'static str {
        "gray"
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
            Some(PixelFormat::Gray8)
        } else {
            None
        }
    }

    fn spatial_dependency(&self) -> SpatialDependency {
        SpatialDependency::Local
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    fn run(&mut self, inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError> {
        let input = inputs
            .get("in")
            .ok_or_else(|| ProcessError::Internal("gray ran without input".to_string()))?;
        let mut data = Vec::with_capacity(input.width() as usize * input.height() as usize);
        for y in 0..input.height() {
            for x in 0..input.width() {
                data.push(input.pixel(x, y)[0]);
            }
        }
        let out = Frame::new(input.width(), input.height(), PixelFormat::Gray8, data)
            .map_err(|e| ProcessError::Internal(e.to_string()))?
            .with_timestamp(input.timestamp())
            .with_sequence(input.sequence());
        let mut outputs = NodeOutputs::new();
        outputs.add("out", out);
        Ok(outputs)
    }
}

/// Test node that produces the wrong outputs, by mode: 0
/// produces nothing, 1 produces an undeclared port, 2 produces
/// its port twice.
struct MisbehavingNode {
    mode: u8,
}

impl Node for MisbehavingNode {
    fn name(&self) -> &'static str {
        "misbehaving"
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
        SpatialDependency::Local
    }

    fn temporal_dependency(&self) -> TemporalDependency {
        TemporalDependency::Stateless
    }

    fn run(&mut self, inputs: &NodeInputs<'_>) -> Result<NodeOutputs, ProcessError> {
        let input = inputs
            .get("in")
            .ok_or_else(|| ProcessError::Internal("misbehaving ran without input".to_string()))?;
        let mut outputs = NodeOutputs::new();
        match self.mode {
            0 => {}
            1 => outputs.add("nope", input.clone()),
            _ => {
                outputs.add("out", input.clone());
                outputs.add("out", input.clone());
            }
        }
        Ok(outputs)
    }
}

fn doubler_node() -> ProcessorNode<Doubler> {
    ProcessorNode::new(Doubler, 2, SpatialDependency::Local)
}

#[test]
fn chain_runs_two_processors_and_propagates_metadata() -> Result<(), String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("first", Box::new(doubler_node()));
    builder.add_node("second", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "first", "in");
    builder.connect(Source::node("first", "out"), "second", "in");
    builder.add_output("out", "second", "out");
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    let frame = rgb_frame(2, 1, vec![1, 2, 3, 4, 5, 6])?.with_sequence(9);
    let outputs = graph.run(vec![("in", frame)]).map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].0, "out");
    let out = &outputs[0].1;
    assert_eq!((out.width(), out.height()), (8, 4));
    assert_eq!(out.sequence(), 9);
    assert_eq!(out.pixel(0, 0), &[1, 2, 3]);
    assert_eq!(out.pixel(7, 3), &[4, 5, 6]);
    Ok(())
}

#[test]
fn branch_feeds_two_nodes_from_one_input() -> Result<(), String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node(
        "copy",
        Box::new(ProcessorNode::new(Identity, 2, SpatialDependency::Local)),
    );
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "copy", "in");
    builder.connect(Source::input("in"), "double", "in");
    builder.add_output("plain", "copy", "out");
    builder.add_output("big", "double", "out");
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    let frame = rgb_frame(2, 1, vec![1, 2, 3, 4, 5, 6])?;
    let outputs = graph.run(vec![("in", frame)]).map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 2);
    let mut plain: Option<Frame> = None;
    let mut big: Option<Frame> = None;
    for (name, frame) in outputs {
        match name.as_str() {
            "plain" => plain = Some(frame),
            "big" => big = Some(frame),
            _ => return Err(format!("unexpected output {name}")),
        }
    }
    let plain = plain.ok_or_else(|| "missing plain output".to_string())?;
    let big = big.ok_or_else(|| "missing big output".to_string())?;
    assert_eq!((plain.width(), plain.height()), (2, 1));
    assert_eq!(plain.data(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!((big.width(), big.height()), (4, 2));
    assert_eq!(big.pixel(3, 1), &[4, 5, 6]);
    Ok(())
}

#[test]
fn two_input_node_blends_two_graph_inputs() -> Result<(), String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("red", PixelFormat::Rgb8);
    builder.add_input("blue", PixelFormat::Rgb8);
    builder.add_node("blend", Box::new(BlendNode));
    builder.connect(Source::input("red"), "blend", "a");
    builder.connect(Source::input("blue"), "blend", "b");
    builder.add_output("out", "blend", "out");
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    let red = rgb_frame(1, 1, vec![200, 0, 0])?;
    let blue = rgb_frame(1, 1, vec![0, 0, 200])?;
    let outputs = graph
        .run(vec![("red", red), ("blue", blue)])
        .map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].1.pixel(0, 0), &[100, 0, 100]);
    Ok(())
}

#[test]
fn two_input_node_blends_two_branches() -> Result<(), String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("left", Box::new(doubler_node()));
    builder.add_node("right", Box::new(doubler_node()));
    builder.add_node("blend", Box::new(BlendNode));
    builder.connect(Source::input("in"), "left", "in");
    builder.connect(Source::input("in"), "right", "in");
    builder.connect(Source::node("left", "out"), "blend", "a");
    builder.connect(Source::node("right", "out"), "blend", "b");
    builder.add_output("out", "blend", "out");
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    let frame = rgb_frame(1, 1, vec![10, 20, 30])?;
    let outputs = graph.run(vec![("in", frame)]).map_err(|e| e.to_string())?;
    // Both branches doubled the same frame, so blending them
    // returns the doubled frame unchanged.
    assert_eq!(
        outputs[0].1.data(),
        vec![10, 20, 30, 10, 20, 30, 10, 20, 30, 10, 20, 30]
    );
    Ok(())
}

#[test]
fn executor_packs_strided_input_for_tight_only_node() -> Result<(), String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node(
        "tight",
        Box::new(ProcessorNode::new(
            TightDoubler,
            2,
            SpatialDependency::Local,
        )),
    );
    builder.connect(Source::input("in"), "tight", "in");
    builder.add_output("out", "tight", "out");
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    let tight = rgb_frame(2, 1, vec![1, 2, 3, 4, 5, 6])?;
    let strided = Frame::new_with_stride(2, 1, PixelFormat::Rgb8, 8, vec![1, 2, 3, 4, 5, 6, 0, 0])
        .map_err(|e| e.to_string())?;
    let from_tight = graph.run(vec![("in", tight)]).map_err(|e| e.to_string())?;
    let from_strided = graph
        .run(vec![("in", strided)])
        .map_err(|e| e.to_string())?;
    assert_eq!(from_strided[0].1.data(), from_tight[0].1.data());
    Ok(())
}

#[test]
fn declarations_are_visible_on_processor_nodes() {
    let local = doubler_node();
    assert_eq!(local.name(), "doubler");
    assert_eq!(local.input_ports(), &["in"]);
    assert_eq!(local.output_ports(), &["out"]);
    assert_eq!(local.input_format("in"), Some(PixelFormat::Rgb8));
    assert_eq!(local.output_format("out"), Some(PixelFormat::Rgb8));
    assert_eq!(local.spatial_dependency(), SpatialDependency::Local);
    assert_eq!(local.temporal_dependency(), TemporalDependency::Stateless);
    assert_eq!(local.stride_support(), StrideSupport::Any);
    let tight = ProcessorNode::new(TightDoubler, 2, SpatialDependency::Local);
    assert_eq!(tight.stride_support(), StrideSupport::TightOnly);
}

#[test]
fn build_rejects_mismatched_format() {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Gray8);
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "double", "in");
    builder.add_output("out", "double", "out");
    assert_eq!(
        builder.build().map(|_| ()),
        Err(GraphError::MismatchedFormat {
            from: "in".to_string(),
            from_port: String::new(),
            to: "double".to_string(),
            to_port: "in".to_string(),
            expected: PixelFormat::Rgb8,
            found: PixelFormat::Gray8,
        })
    );
}

#[test]
fn build_rejects_gray_producer_feeding_rgb_node() {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("gray", Box::new(GrayNode));
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "gray", "in");
    builder.connect(Source::node("gray", "out"), "double", "in");
    builder.add_output("out", "double", "out");
    assert!(matches!(
        builder.build().map(|_| ()),
        Err(GraphError::MismatchedFormat { .. })
    ));
}

#[test]
fn build_rejects_cycle() {
    // A real cycle needs every port fed exactly once: the blend
    // node takes the graph input on "a" and the doubler's output
    // on "b", while the doubler takes the blend node's output.
    // The first attempt at this test wired the cycle through a
    // port that already had an edge, which is an
    // InputAlreadyConnected error, not a cycle.
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("first", Box::new(BlendNode));
    builder.add_node("second", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "first", "a");
    builder.connect(Source::node("second", "out"), "first", "b");
    builder.connect(Source::node("first", "out"), "second", "in");
    builder.add_output("out", "second", "out");
    let result = builder.build().map(|_| ());
    assert!(
        matches!(&result, Err(GraphError::Cycle(nodes))
            if nodes.contains(&"first".to_string())
                && nodes.contains(&"second".to_string())),
        "expected a cycle over first and second, got {result:?}"
    );
}

#[test]
fn build_rejects_unconnected_input() {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("double", Box::new(doubler_node()));
    builder.add_output("out", "double", "out");
    assert_eq!(
        builder.build().map(|_| ()),
        Err(GraphError::InputUnconnected {
            node: "double".to_string(),
            port: "in".to_string(),
        })
    );
}

#[test]
fn build_rejects_twice_connected_input() {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "double", "in");
    builder.connect(Source::input("in"), "double", "in");
    builder.add_output("out", "double", "out");
    assert_eq!(
        builder.build().map(|_| ()),
        Err(GraphError::InputAlreadyConnected {
            node: "double".to_string(),
            port: "in".to_string(),
        })
    );
}

#[test]
fn build_rejects_duplicate_node_and_unknown_names() {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("double", Box::new(doubler_node()));
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "double", "in");
    builder.add_output("out", "double", "out");
    assert_eq!(
        builder.build().map(|_| ()),
        Err(GraphError::DuplicateNode("double".to_string()))
    );

    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "ghost", "in");
    builder.add_output("out", "double", "out");
    assert_eq!(
        builder.build().map(|_| ()),
        Err(GraphError::UnknownNode("ghost".to_string()))
    );

    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("double", Box::new(doubler_node()));
    builder.connect(Source::input("in"), "double", "sideways");
    builder.add_output("out", "double", "out");
    assert_eq!(
        builder.build().map(|_| ()),
        Err(GraphError::UnknownPort {
            node: "double".to_string(),
            port: "sideways".to_string(),
        })
    );
}

#[test]
fn run_rejects_missing_unexpected_and_duplicate_inputs() -> Result<(), String> {
    let build = || {
        let mut builder = GraphBuilder::new();
        builder.add_input("in", PixelFormat::Rgb8);
        builder.add_node("double", Box::new(doubler_node()));
        builder.connect(Source::input("in"), "double", "in");
        builder.add_output("out", "double", "out");
        builder.build().map_err(|e| e.to_string())
    };
    let frame = rgb_frame(1, 1, vec![1, 2, 3])?;
    let mut graph = build()?;
    assert_eq!(
        graph.run(vec![]).map(|_| ()),
        Err(GraphError::MissingGraphInput("in".to_string()))
    );
    let mut graph = build()?;
    assert_eq!(
        graph
            .run(vec![("in", frame.clone()), ("other", frame.clone())])
            .map(|_| ()),
        Err(GraphError::UnexpectedGraphInput("other".to_string()))
    );
    let mut graph = build()?;
    assert_eq!(
        graph
            .run(vec![("in", frame.clone()), ("in", frame.clone())])
            .map(|_| ()),
        Err(GraphError::DuplicateGraphInput("in".to_string()))
    );
    let mut graph = build()?;
    let gray = Frame::new(1, 1, PixelFormat::Gray8, vec![7]).map_err(|e| e.to_string())?;
    assert_eq!(
        graph.run(vec![("in", gray)]).map(|_| ()),
        Err(GraphError::InputFormatMismatch {
            input: "in".to_string(),
            expected: PixelFormat::Rgb8,
            found: PixelFormat::Gray8,
        })
    );
    Ok(())
}

#[test]
fn failing_node_fails_the_run_with_its_name() -> Result<(), String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node("fail", Box::new(FailNode));
    builder.connect(Source::input("in"), "fail", "in");
    builder.add_output("out", "fail", "out");
    let mut graph = builder.build().map_err(|e| e.to_string())?;
    let frame = rgb_frame(1, 1, vec![1, 2, 3])?;
    let result = graph.run(vec![("in", frame)]);
    assert!(matches!(
        result,
        Err(GraphError::NodeFailed { node, .. }) if node == "fail"
    ));
    Ok(())
}

#[test]
fn misbehaving_nodes_are_rejected() -> Result<(), String> {
    for mode in [0u8, 1, 2] {
        let mut builder = GraphBuilder::new();
        builder.add_input("in", PixelFormat::Rgb8);
        builder.add_node("bad", Box::new(MisbehavingNode { mode }));
        builder.connect(Source::input("in"), "bad", "in");
        builder.add_output("out", "bad", "out");
        let mut graph = builder.build().map_err(|e| e.to_string())?;
        let frame = rgb_frame(1, 1, vec![1, 2, 3])?;
        let result = graph.run(vec![("in", frame)]).map(|_| ());
        match mode {
            0 => assert_eq!(
                result,
                Err(GraphError::NodeOutputMissing {
                    node: "bad".to_string(),
                    port: "out".to_string(),
                })
            ),
            1 => assert_eq!(
                result,
                Err(GraphError::NodeOutputUnknown {
                    node: "bad".to_string(),
                    port: "nope".to_string(),
                })
            ),
            _ => assert_eq!(
                result,
                Err(GraphError::NodeOutputDuplicate {
                    node: "bad".to_string(),
                    port: "out".to_string(),
                })
            ),
        }
    }
    Ok(())
}

/// One-node graph over the tile-safe `Doubler` at the given
/// spatial dependency, for the incremental tests.
fn incremental_graph(dependency: SpatialDependency) -> Result<Graph, String> {
    let mut builder = GraphBuilder::new();
    builder.add_input("in", PixelFormat::Rgb8);
    builder.add_node(
        "double",
        Box::new(ProcessorNode::new(Doubler, 2, dependency)),
    );
    builder.connect(Source::input("in"), "double", "in");
    builder.add_output("out", "double", "out");
    builder.build().map_err(|e| e.to_string())
}

/// A frame whose pixels vary by position, so a changed pixel is
/// a real change and the doubled output is not uniform.
fn pattern_frame(width: u32, height: u32) -> Result<Frame, String> {
    let mut data = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let v = ((x * 7 + y * 13) % 251) as u8;
            data.extend_from_slice(&[v, (v / 2) as u8, (255 - v) as u8]);
        }
    }
    rgb_frame(width, height, data)
}

/// The same pattern frame with one pixel replaced by a value
/// that differs from the pattern there.
fn pattern_frame_changed(width: u32, height: u32, x: u32, y: u32) -> Result<Frame, String> {
    let mut frame = pattern_frame(width, height)?;
    let old = frame.pixel(x, y).to_vec();
    let new = [
        old[0].wrapping_add(100),
        old[1].wrapping_add(100),
        old[2].wrapping_add(100),
    ];
    frame.pixel_mut(x, y).copy_from_slice(&new);
    Ok(frame)
}

/// Whole-frame reference for a frame: a fresh graph's `run`.
fn whole_frame_output(frame: Frame, dependency: SpatialDependency) -> Result<Frame, String> {
    let mut graph = incremental_graph(dependency)?;
    let mut outputs = graph.run(vec![("in", frame)]).map_err(|e| e.to_string())?;
    assert_eq!(outputs.len(), 1);
    Ok(outputs.remove(0).1)
}

#[test]
fn incremental_first_run_matches_whole_frame_run() -> Result<(), String> {
    for dependency in [SpatialDependency::Local, SpatialDependency::Global] {
        let frame = pattern_frame(32, 32)?;
        let expected = whole_frame_output(frame.clone(), dependency)?;
        let mut graph = incremental_graph(dependency)?;
        let outputs = graph
            .run_incremental(vec![("in", frame)])
            .map_err(|e| e.to_string())?;
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].1.data(), expected.data());
    }
    Ok(())
}

#[test]
fn incremental_changed_tile_matches_whole_frame_run() -> Result<(), String> {
    for dependency in [SpatialDependency::Local, SpatialDependency::Global] {
        let first = pattern_frame(32, 32)?;
        // One pixel changed inside tile (1, 0), and one on the
        // tile boundary at (16, 0): both must land in exactly
        // the changed tiles' output.
        let second = pattern_frame_changed(32, 32, 20, 5)?;
        let third = pattern_frame_changed(32, 32, 16, 0)?;
        let expected_second = whole_frame_output(second.clone(), dependency)?;
        let expected_third = whole_frame_output(third.clone(), dependency)?;
        let mut graph = incremental_graph(dependency)?;
        let out_first = graph
            .run_incremental(vec![("in", first)])
            .map_err(|e| e.to_string())?;
        assert_eq!(
            out_first[0].1.data(),
            whole_frame_output(pattern_frame(32, 32)?, dependency)?.data()
        );
        let out_second = graph
            .run_incremental(vec![("in", second)])
            .map_err(|e| e.to_string())?;
        assert_eq!(out_second[0].1.data(), expected_second.data());
        let out_third = graph
            .run_incremental(vec![("in", third)])
            .map_err(|e| e.to_string())?;
        assert_eq!(out_third[0].1.data(), expected_third.data());
    }
    Ok(())
}

#[test]
fn incremental_changed_edge_tile_matches_whole_frame_run() -> Result<(), String> {
    // 20x20: the edge tiles are partial (4 pixels wide), and a
    // change in the bottom-right edge tile must still splice
    // exactly.
    let first = pattern_frame(20, 20)?;
    let second = pattern_frame_changed(20, 20, 19, 19)?;
    let expected = whole_frame_output(second.clone(), SpatialDependency::Local)?;
    let mut graph = incremental_graph(SpatialDependency::Local)?;
    graph
        .run_incremental(vec![("in", first)])
        .map_err(|e| e.to_string())?;
    let outputs = graph
        .run_incremental(vec![("in", second)])
        .map_err(|e| e.to_string())?;
    assert_eq!(outputs[0].1.data(), expected.data());
    Ok(())
}

#[test]
fn incremental_static_frame_matches_whole_frame_run() -> Result<(), String> {
    for dependency in [SpatialDependency::Local, SpatialDependency::Global] {
        let frame = pattern_frame(32, 32)?;
        let expected = whole_frame_output(frame.clone(), dependency)?;
        let mut graph = incremental_graph(dependency)?;
        graph
            .run_incremental(vec![("in", frame.clone())])
            .map_err(|e| e.to_string())?;
        // Same pixels, fresh frame: nothing is dirty, so the
        // node is skipped and the cached output stands.
        let outputs = graph
            .run_incremental(vec![("in", frame)])
            .map_err(|e| e.to_string())?;
        assert_eq!(outputs[0].1.data(), expected.data());
    }
    Ok(())
}
