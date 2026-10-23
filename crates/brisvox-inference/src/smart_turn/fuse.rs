//! A cheaper graph for the same Smart Turn model: each exact (erf) GELU, exported as five
//! element-wise passes over a `[400, 1536]` float tensor (`x / √2`, `erf`, `+ 1`, `· 0.5`, `· x`),
//! becomes one `com.microsoft.Gelu` node, which ONNX Runtime runs as a single pass and fuses with
//! the preceding bias add (`BiasGelu`). The pinned file is unchanged on disk; the rewrite happens in
//! memory when a session is created.
//!
//! Why: an analysis is bound by memory traffic, not arithmetic (about 330 MB moved per analysis
//! with the exported graph, about 200 MB with this one; docs/LATENCY.md "Capacity with the real
//! provider path"). The math is the same function, so the probabilities move only by the rounding
//! of `Gelu`'s erf against the separate `Erf` kernel, amplified at most to the next quantization
//! step of the int8 graph: on the 4000-clip sample of the Smart Turn v3.2 test set the verdicts
//! matched the exported graph on 3997 clips (accuracy against the labels 92.70% against 92.67%),
//! docs/providers/smart-turn.md "Fused GELU".
//!
//! Only the exact pattern is rewritten: `Div(x, √2) → Erf → Add(·, 1) → Mul(0.5, ·) → Mul(x, ·)`
//! (operand order free), with constant operands that hold exactly those values and intermediate
//! results that nothing else reads. Anything else is left as it is.

use prost::Message;
use tract_onnx::pb::{AttributeProto, ModelProto, NodeProto, OperatorSetIdProto, TensorProto};

use crate::error::{Error, Result};

/// The domain of ONNX Runtime's contrib operators, where `Gelu` lives.
const MS_DOMAIN: &str = "com.microsoft";

/// The result of [`fuse_gelu`]: the rewritten model and how many GELUs it replaced.
#[derive(Debug)]
pub struct Fused {
    /// The serialized model to hand to ONNX Runtime.
    pub bytes: Vec<u8>,
    /// GELU subgraphs replaced by one `Gelu` node each.
    pub gelus: usize,
}

/// Rewrites every exact erf GELU of the model in `onnx` into `com.microsoft.Gelu`.
///
/// # Errors
/// [`Error::Model`] if the bytes are not an ONNX model.
pub fn fuse_gelu(onnx: &[u8]) -> Result<Fused> {
    let mut model =
        ModelProto::decode(onnx).map_err(|e| Error::Model(format!("not an ONNX model: {e}")))?;
    let Some(graph) = model.graph.as_mut() else {
        return Err(Error::Model("ONNX model without a graph".to_owned()));
    };
    // A node with a subgraph (If, Loop, Scan) can read names of this graph from inside it, where
    // the reader check below does not look: leave such a model as it is.
    if graph.node.iter().any(|n| {
        n.attribute
            .iter()
            .any(|a| a.g.is_some() || !a.graphs.is_empty())
    }) {
        return Ok(Fused {
            bytes: model.encode_to_vec(),
            gelus: 0,
        });
    }
    let constants = Constants::of(graph.initializer.as_slice(), graph.node.as_slice());
    let mut replace: Vec<(usize, Vec<usize>, NodeProto)> = Vec::new();
    for (erf_at, erf) in graph.node.iter().enumerate() {
        if erf.op_type != "Erf" || !erf.domain.is_empty() {
            continue;
        }
        if let Some((chain, gelu)) = match_gelu(
            graph.node.as_slice(),
            &graph.output_names(),
            &constants,
            erf_at,
            replace.len(),
        ) {
            replace.push((erf_at, chain, gelu));
        }
    }
    let gelus = replace.len();
    if gelus > 0 {
        let mut drop = vec![false; graph.node.len()];
        let mut put: Vec<Option<NodeProto>> = vec![None; graph.node.len()];
        // The intermediates of the replaced chains (every output but the chain's last), whose
        // shape annotations go with them.
        let mut gone: Vec<String> = Vec::new();
        // Everything the replaced nodes read: their constants may have no reader left.
        let mut chain_inputs: Vec<String> = Vec::new();
        for (_, chain, gelu) in replace {
            // The Gelu takes the place of the chain's last node: its input exists before the chain
            // and its output's readers come after it, so the graph stays topologically sorted.
            let last = chain.iter().copied().max().unwrap_or(0);
            for i in chain {
                if let Some(d) = drop.get_mut(i) {
                    *d = true;
                }
                if let Some(node) = graph.node.get(i) {
                    chain_inputs.extend(node.input.iter().cloned());
                }
                if i != last
                    && let Some(node) = graph.node.get(i)
                {
                    gone.extend(node.output.iter().cloned());
                }
            }
            if let Some(slot) = put.get_mut(last) {
                *slot = Some(gelu);
            }
        }
        let nodes = std::mem::take(&mut graph.node);
        graph.node = nodes
            .into_iter()
            .zip(drop.into_iter().zip(put))
            .filter_map(|(node, (dropped, gelu))| if dropped { gelu } else { Some(node) })
            .collect();
        graph.value_info.retain(|v| !gone.contains(&v.name));
        // The chains' constants (√2, 1, 0.5) that nothing reads any more.
        let read: std::collections::HashSet<&str> = graph
            .node
            .iter()
            .flat_map(|n| n.input.iter())
            .chain(graph.output.iter().map(|o| &o.name))
            .map(String::as_str)
            .collect();
        let unread: Vec<String> = chain_inputs
            .into_iter()
            .filter(|name| !read.contains(name.as_str()))
            .collect();
        graph.initializer.retain(|t| !unread.contains(&t.name));
        graph.node.retain(|n| {
            !(n.op_type == "Constant"
                && !n.output.is_empty()
                && n.output.iter().all(|o| unread.contains(o)))
        });
        if !model.opset_import.iter().any(|o| o.domain == MS_DOMAIN) {
            model.opset_import.push(OperatorSetIdProto {
                domain: MS_DOMAIN.to_owned(),
                version: 1,
            });
        }
    }
    Ok(Fused {
        bytes: model.encode_to_vec(),
        gelus,
    })
}

/// The model decoded and encoded again without a change: what [`fuse_gelu`] does around its
/// rewrite. For tests and diagnostics (it shows the protobuf types keep every field of a model).
///
/// # Errors
/// [`Error::Model`] if the bytes are not an ONNX model.
pub fn reencode(onnx: &[u8]) -> Result<Vec<u8>> {
    let model =
        ModelProto::decode(onnx).map_err(|e| Error::Model(format!("not an ONNX model: {e}")))?;
    Ok(model.encode_to_vec())
}

trait OutputNames {
    fn output_names(&self) -> Vec<&str>;
}

impl OutputNames for tract_onnx::pb::GraphProto {
    fn output_names(&self) -> Vec<&str> {
        self.output.iter().map(|o| o.name.as_str()).collect()
    }
}

/// Scalar float constants of the graph: initializers and `Constant` nodes.
struct Constants<'a> {
    initializers: &'a [TensorProto],
    nodes: &'a [NodeProto],
}

impl<'a> Constants<'a> {
    fn of(initializers: &'a [TensorProto], nodes: &'a [NodeProto]) -> Self {
        Self {
            initializers,
            nodes,
        }
    }

    /// The value of `name` if it is a constant holding exactly one float.
    fn scalar(&self, name: &str) -> Option<f32> {
        if let Some(t) = self.initializers.iter().find(|t| t.name == name) {
            return scalar_of(t);
        }
        let node = self
            .nodes
            .iter()
            .find(|n| n.op_type == "Constant" && n.output.iter().any(|o| o == name))?;
        let value: &AttributeProto = node.attribute.iter().find(|a| a.name == "value")?;
        value.t.as_ref().and_then(scalar_of)
    }
}

/// One float from a float tensor of a single element and rank 0 or 1 (`float_data` or
/// little-endian `raw_data`). A higher rank would broadcast `x` to more dimensions, which the
/// replacing `Gelu` would not.
fn scalar_of(t: &TensorProto) -> Option<f32> {
    const FLOAT: i32 = 1;
    if t.data_type != FLOAT || t.dims.len() > 1 || t.dims.iter().product::<i64>() != 1 {
        return None;
    }
    if let [v] = t.float_data.as_slice() {
        return Some(*v);
    }
    let raw: [u8; 4] = t.raw_data.as_slice().try_into().ok()?;
    Some(f32::from_le_bytes(raw))
}

/// Indices of the nodes that read `name`.
fn readers(nodes: &[NodeProto], name: &str) -> Vec<usize> {
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.input.iter().any(|i| i == name))
        .map(|(i, _)| i)
        .collect()
}

fn producer(nodes: &[NodeProto], name: &str) -> Option<usize> {
    nodes
        .iter()
        .position(|n| n.output.iter().any(|o| o == name))
}

/// The one node reading `name` (and nothing else reads it, and it is not a graph output).
fn sole_reader(nodes: &[NodeProto], outputs: &[&str], name: &str) -> Option<usize> {
    if outputs.contains(&name) {
        return None;
    }
    match readers(nodes, name).as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// The other input of a binary node, given one of its inputs.
fn other_input<'n>(node: &'n NodeProto, one: &str) -> Option<&'n str> {
    match node.input.as_slice() {
        [a, b] if a == one => Some(b.as_str()),
        [a, b] if b == one => Some(a.as_str()),
        _ => None,
    }
}

fn is_op(node: &NodeProto, op: &str) -> bool {
    node.op_type == op && node.domain.is_empty()
}

/// Matches `Div(x, √2) → Erf → Add(·, 1) → Mul(0.5, ·) → Mul(x, ·)` around the `Erf` at `erf_at`:
/// the chain's node indices and the replacing `Gelu`.
fn match_gelu(
    nodes: &[NodeProto],
    outputs: &[&str],
    constants: &Constants<'_>,
    erf_at: usize,
    ordinal: usize,
) -> Option<(Vec<usize>, NodeProto)> {
    let close = |v: f32, want: f32| (v - want).abs() <= want.abs() * 1e-6;
    let erf = nodes.get(erf_at)?;
    let div_at = producer(nodes, erf.input.first()?)?;
    let div = nodes.get(div_at)?;
    if !is_op(div, "Div") || sole_reader(nodes, outputs, div.output.first()?)? != erf_at {
        return None;
    }
    let (x, sqrt2) = (div.input.first()?, div.input.get(1)?);
    if !close(constants.scalar(sqrt2)?, std::f32::consts::SQRT_2) {
        return None;
    }
    let erf_out = erf.output.first()?;
    let add_at = sole_reader(nodes, outputs, erf_out)?;
    let add = nodes.get(add_at)?;
    if !is_op(add, "Add") || !close(constants.scalar(other_input(add, erf_out)?)?, 1.0) {
        return None;
    }
    let add_out = add.output.first()?;
    let half_at = sole_reader(nodes, outputs, add_out)?;
    let half = nodes.get(half_at)?;
    if !is_op(half, "Mul") || !close(constants.scalar(other_input(half, add_out)?)?, 0.5) {
        return None;
    }
    let half_out = half.output.first()?;
    let times_x_at = sole_reader(nodes, outputs, half_out)?;
    let times_x = nodes.get(times_x_at)?;
    if !is_op(times_x, "Mul") || other_input(times_x, half_out)? != x {
        return None;
    }
    let gelu = NodeProto {
        input: vec![x.clone()],
        output: times_x.output.clone(),
        name: format!("brisvox_fused_gelu_{ordinal}"),
        op_type: "Gelu".to_owned(),
        domain: MS_DOMAIN.to_owned(),
        ..NodeProto::default()
    };
    Some((vec![div_at, erf_at, add_at, half_at, times_x_at], gelu))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tract_onnx::pb::GraphProto;

    fn scalar(name: &str, v: f32) -> TensorProto {
        TensorProto {
            name: name.to_owned(),
            data_type: 1,
            raw_data: v.to_le_bytes().to_vec(),
            ..TensorProto::default()
        }
    }

    fn node(op: &str, inputs: &[&str], output: &str) -> NodeProto {
        NodeProto {
            input: inputs.iter().map(|s| (*s).to_owned()).collect(),
            output: vec![output.to_owned()],
            op_type: op.to_owned(),
            ..NodeProto::default()
        }
    }

    fn model(nodes: Vec<NodeProto>, sqrt2: f32) -> Vec<u8> {
        let graph = GraphProto {
            node: nodes,
            initializer: vec![scalar("s2", sqrt2), scalar("one", 1.0), scalar("half", 0.5)],
            output: vec![tract_onnx::pb::ValueInfoProto {
                name: "y".to_owned(),
                ..Default::default()
            }],
            ..GraphProto::default()
        };
        ModelProto {
            graph: Some(graph),
            ..ModelProto::default()
        }
        .encode_to_vec()
    }

    fn gelu_chain() -> Vec<NodeProto> {
        vec![
            node("Div", &["x", "s2"], "d"),
            node("Erf", &["d"], "e"),
            node("Add", &["e", "one"], "a"),
            node("Mul", &["half", "a"], "h"),
            node("Mul", &["x", "h"], "y"),
        ]
    }

    #[test]
    fn the_exported_gelu_becomes_one_node() {
        let fused = fuse_gelu(&model(gelu_chain(), std::f32::consts::SQRT_2)).unwrap();
        assert_eq!(fused.gelus, 1);
        let m = ModelProto::decode(fused.bytes.as_slice()).unwrap();
        let g = m.graph.unwrap();
        assert_eq!(g.node.len(), 1);
        assert_eq!(
            (g.node[0].op_type.as_str(), g.node[0].domain.as_str()),
            ("Gelu", MS_DOMAIN)
        );
        assert_eq!(
            (g.node[0].input.clone(), g.node[0].output.clone()),
            (vec!["x".to_owned()], vec!["y".to_owned()])
        );
        assert!(m.opset_import.iter().any(|o| o.domain == MS_DOMAIN));
    }

    #[test]
    fn a_different_constant_or_a_shared_intermediate_is_left_alone() {
        // Not √2: a different function.
        assert_eq!(fuse_gelu(&model(gelu_chain(), 1.5)).unwrap().gelus, 0);
        // The erf result is read twice: rewriting would lose a value someone needs.
        let mut nodes = gelu_chain();
        nodes.push(node("Neg", &["e"], "z"));
        assert_eq!(
            fuse_gelu(&model(nodes, std::f32::consts::SQRT_2))
                .unwrap()
                .gelus,
            0
        );
        // The last multiply is by something other than x.
        let mut nodes = gelu_chain();
        nodes[4] = node("Mul", &["w", "h"], "y");
        assert_eq!(
            fuse_gelu(&model(nodes, std::f32::consts::SQRT_2))
                .unwrap()
                .gelus,
            0
        );
    }

    fn fused_nodes(
        nodes: Vec<NodeProto>,
        extra: Vec<TensorProto>,
        outputs: &[&str],
    ) -> (usize, Vec<NodeProto>) {
        let mut initializer = vec![
            scalar("s2", std::f32::consts::SQRT_2),
            scalar("one", 1.0),
            scalar("half", 0.5),
        ];
        initializer.extend(extra);
        let graph = GraphProto {
            node: nodes,
            initializer,
            output: outputs
                .iter()
                .map(|n| tract_onnx::pb::ValueInfoProto {
                    name: (*n).to_owned(),
                    ..Default::default()
                })
                .collect(),
            ..GraphProto::default()
        };
        let bytes = ModelProto {
            graph: Some(graph),
            ..ModelProto::default()
        }
        .encode_to_vec();
        let fused = fuse_gelu(&bytes).unwrap();
        let graph = ModelProto::decode(fused.bytes.as_slice())
            .unwrap()
            .graph
            .unwrap();
        (fused.gelus, graph.node)
    }

    fn ops(nodes: &[NodeProto]) -> Vec<&str> {
        nodes.iter().map(|n| n.op_type.as_str()).collect()
    }

    #[test]
    fn a_constant_node_and_swapped_operands_are_matched() {
        let constant = NodeProto {
            output: vec!["k1".to_owned()],
            op_type: "Constant".to_owned(),
            attribute: vec![AttributeProto {
                name: "value".to_owned(),
                t: Some(scalar("", 1.0)),
                ..AttributeProto::default()
            }],
            ..NodeProto::default()
        };
        let nodes = vec![
            constant,
            node("Div", &["x", "s2"], "d"),
            node("Erf", &["d"], "e"),
            node("Add", &["k1", "e"], "a"),
            node("Mul", &["a", "half"], "h"),
            node("Mul", &["h", "x"], "y"),
        ];
        let (gelus, nodes) = fused_nodes(nodes, vec![], &["y"]);
        assert_eq!(gelus, 1);
        assert_eq!(
            ops(&nodes),
            ["Gelu"],
            "the Constant only fed the chain: pruned"
        );
    }

    #[test]
    fn an_intermediate_that_is_a_graph_output_or_a_reversed_div_is_left_alone() {
        let (gelus, _) = fused_nodes(gelu_chain(), vec![], &["y", "a"]);
        assert_eq!(gelus, 0, "the + 1 result is a graph output");
        let mut nodes = gelu_chain();
        nodes[0] = node("Div", &["s2", "x"], "d");
        assert_eq!(
            fused_nodes(nodes, vec![], &["y"]).0,
            0,
            "sqrt2 / x is another function"
        );
        // A constant of rank 2 broadcasts x to more dimensions: not the same function.
        let mut wide = scalar("s2w", std::f32::consts::SQRT_2);
        wide.dims = vec![1, 1];
        let mut nodes = gelu_chain();
        nodes[0] = node("Div", &["x", "s2w"], "d");
        assert_eq!(fused_nodes(nodes, vec![wide], &["y"]).0, 0);
    }

    #[test]
    fn interleaved_and_stacked_gelus_keep_the_graph_in_order() {
        // Unrelated nodes inside the chain, and a second GELU on the first one's output.
        let nodes = vec![
            node("Div", &["x", "s2"], "d"),
            node("Relu", &["w"], "r"),
            node("Erf", &["d"], "e"),
            node("Add", &["e", "one"], "a"),
            node("Neg", &["r"], "n"),
            node("Mul", &["half", "a"], "h"),
            node("Mul", &["x", "h"], "g1"),
            node("Div", &["g1", "s2"], "d2"),
            node("Erf", &["d2"], "e2"),
            node("Add", &["e2", "one"], "a2"),
            node("Mul", &["half", "a2"], "h2"),
            node("Mul", &["g1", "h2"], "y"),
            node("Add", &["y", "n"], "z"),
        ];
        let (gelus, nodes) = fused_nodes(nodes, vec![], &["z"]);
        assert_eq!(gelus, 2);
        assert_eq!(ops(&nodes), ["Relu", "Neg", "Gelu", "Gelu", "Add"]);
        // Every input is produced before it is read (or is a graph input / initializer).
        let mut known: Vec<String> = ["x", "w", "s2", "one", "half"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        for n in &nodes {
            for i in &n.input {
                assert!(
                    known.contains(i),
                    "{} reads {i} before it exists",
                    n.op_type
                );
            }
            known.extend(n.output.iter().cloned());
        }
        assert_eq!(
            (nodes[2].input[0].as_str(), nodes[2].output[0].as_str()),
            ("x", "g1")
        );
        assert_eq!(
            (nodes[3].input[0].as_str(), nodes[3].output[0].as_str()),
            ("g1", "y")
        );
    }

    #[test]
    fn garbage_is_an_error_and_the_rewrite_is_deterministic() {
        assert!(fuse_gelu(b"not a model").is_err());
        let m = model(gelu_chain(), std::f32::consts::SQRT_2);
        assert_eq!(fuse_gelu(&m).unwrap().bytes, fuse_gelu(&m).unwrap().bytes);
    }
}
