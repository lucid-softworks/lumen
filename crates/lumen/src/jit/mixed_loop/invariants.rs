//! Conditional object provenance, used to select profitable live identity-checked probes.
//! This analysis does not replace runtime Object/descriptor/prototype guards.
use crate::{
    bytecode::{Chunk, Op},
    jit_ir::{FrameLoc, Inst, InstKind, RegionIr, ValueId},
};
use std::{
    collections::{BTreeSet, HashMap},
    rc::Rc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct NodeId(pub usize);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Node {
    This,
    EntryLocal(u16),
    OwnObject { receiver: NodeId, name: Rc<str> },
    MethodObject { receiver: NodeId, name: Rc<str> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadKind {
    Own,
    Method,
}

#[derive(Clone, Debug)]
pub(super) struct Site {
    pub pc: usize,
    #[cfg(test)]
    pub receiver: NodeId,
    #[cfg(test)]
    pub name: u32,
    #[cfg(test)]
    pub kind: ReadKind,
}

#[derive(Default)]
pub(super) struct Analysis {
    nodes: Vec<Node>,
    values: HashMap<ValueId, NodeId>,
    sites: Vec<Site>,
}

impl Analysis {
    pub fn sites(&self) -> &[Site] {
        &self.sites
    }
    #[cfg(test)]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    fn value_node(&self, value: ValueId) -> Option<NodeId> {
        self.values.get(&value).copied()
    }
    #[cfg(test)]
    pub fn receiver_at(&self, pc: usize) -> Option<NodeId> {
        self.sites.iter().find(|s| s.pc == pc).map(|s| s.receiver)
    }
    fn intern(&mut self, node: Node) -> NodeId {
        if let Some(index) = self.nodes.iter().position(|n| *n == node) {
            return NodeId(index);
        }
        let id = NodeId(self.nodes.len());
        self.nodes.push(node);
        id
    }
    fn assign(&mut self, value: ValueId, node: NodeId) {
        debug_assert!(self.values.get(&value).is_none_or(|old| *old == node));
        self.values.insert(value, node);
    }
}

/// Caller supplies the admitted helper-free, numeric-write-only PC set. Object
/// edges are invariant only conditional on the runtime read proving Object data.
/// Unknown/differing phi inputs never acquire a guessed identity; no SCC guessing.
pub(super) fn analyze(chunk: &Chunk, ir: &RegionIr, admitted: &[usize]) -> Analysis {
    let admitted: BTreeSet<_> = admitted.iter().copied().collect();
    let mut out = Analysis::default();
    let root = out.intern(Node::This);
    out.assign(ir.this_value, root);
    let writes = written_slots(ir, &admitted);
    if let Some(header) = ir.blocks.iter().find(|b| b.cfg_block == ir.header) {
        for &(loc, value) in &header.params {
            if let FrameLoc::Local(slot) = loc {
                if !writes.contains(&slot) {
                    let node = out.intern(Node::EntryLocal(slot));
                    out.assign(value, node);
                }
            }
        }
    }
    // Each useful round assigns at least one previously unknown SSA value.
    for _ in 0..=ir.values.len() {
        let before = out.values.len();
        merge_params(&mut out, ir, &admitted);
        for inst in ir
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|s| admitted.contains(&s.pc))
        {
            propagate(&mut out, chunk, inst);
        }
        if out.values.len() == before {
            break;
        }
    }
    for inst in ir
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|s| admitted.contains(&s.pc))
    {
        let Some((_name, _, _kind)) = read(chunk.jit_ops()[inst.pc]) else {
            continue;
        };
        let Some(_receiver) = inst.inputs.first().and_then(|v| out.value_node(*v)) else {
            continue;
        };
        out.sites.push(Site {
            pc: inst.pc,
            #[cfg(test)]
            receiver: _receiver,
            #[cfg(test)]
            name: _name,
            #[cfg(test)]
            kind: _kind,
        });
    }
    out.sites.sort_by_key(|s| s.pc);
    out.sites.truncate(32);
    out
}

fn written_slots(ir: &RegionIr, admitted: &BTreeSet<usize>) -> BTreeSet<u16> {
    let mut slots = BTreeSet::new();
    for inst in ir
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter(|s| admitted.contains(&s.pc))
    {
        match inst.kind {
            InstKind::StoreLocal(slot) | InstKind::UpdateLocal(slot, _) => {
                slots.insert(slot);
            }
            InstKind::ResetSlots { start, count } => {
                slots.extend(start..start.saturating_add(count));
            }
            _ => {}
        }
    }
    slots
}

fn merge_params(out: &mut Analysis, ir: &RegionIr, admitted: &BTreeSet<usize>) {
    for block in &ir.blocks {
        if block.cfg_block == ir.header {
            continue;
        }
        for (index, &(_, param)) in block.params.iter().enumerate() {
            if out.value_node(param).is_some() {
                continue;
            }
            // A block ending beyond an unsupported opcode never transfers to its
            // successor in the admitted region. Do not count its cold edge.
            let incoming: Vec<_> = ir
                .blocks
                .iter()
                .filter(|b| b.insts.iter().all(|s| admitted.contains(&s.pc)))
                .flat_map(|b| &b.successors)
                .filter(|e| e.target == block.cfg_block)
                .map(|e| e.args.get(index).and_then(|v| out.value_node(*v)))
                .collect();
            let Some(Some(first)) = incoming.first().copied() else {
                continue;
            };
            if incoming.iter().all(|v| *v == Some(first)) {
                out.assign(param, first);
            }
        }
    }
}

fn read(op: Op) -> Option<(u32, u32, ReadKind)> {
    match op {
        Op::GetProp(n, c) | Op::GetPropThis(n, c) | Op::GetPropLocal(_, n, c) => {
            Some((n, c, ReadKind::Own))
        }
        Op::GetMethod(n, c) => Some((n, c, ReadKind::Method)),
        _ => None,
    }
}

fn propagate(out: &mut Analysis, chunk: &Chunk, inst: &Inst) {
    let Some(input) = inst.inputs.first().and_then(|v| out.value_node(*v)) else {
        return;
    };
    if matches!(inst.kind, InstKind::Clone | InstKind::CheckLocal(_)) {
        if let Some(&value) = inst.outputs.first() {
            out.assign(value, input);
        }
        return;
    }
    let Some((name, _, kind)) = read(chunk.jit_ops()[inst.pc]) else {
        return;
    };
    let Some(&result) = inst.outputs.last() else {
        return;
    };
    let name = Rc::from(chunk.jit_name(name));
    let node = out.intern(match kind {
        ReadKind::Own => Node::OwnObject {
            receiver: input,
            name,
        },
        ReadKind::Method => Node::MethodObject {
            receiver: input,
            name,
        },
    });
    if kind == ReadKind::Method {
        if let Some(&receiver) = inst.outputs.first() {
            out.assign(receiver, input);
        }
    }
    out.assign(result, node);
}

#[cfg(test)]
mod tests {
    use super::{analyze, Analysis, Node, ReadKind};
    use crate::{
        ast::Stmt,
        bytecode::{self, Chunk, Op},
        jit_ir::{Cfg, RegionIr},
        parser,
    };
    use std::rc::Rc;

    fn inspect(source: &str) -> (Rc<Chunk>, RegionIr, Analysis) {
        let statements = parser::parse_script(source, false)
            .unwrap_or_else(|_| panic!("valid analysis fixture: {source}"));
        let Stmt::FuncDecl(function) = &statements[0] else {
            panic!("function fixture");
        };
        let chunk = bytecode::compile(function).expect("compiled real loop");
        let cfg = Cfg::build(&chunk).unwrap();
        let header = cfg.loops().first().expect("natural loop").header;
        let head = cfg.blocks()[header.0 as usize].start;
        let plan = super::super::plan::build(&chunk, &cfg, head).expect("admitted mixed loop");
        let ir = RegionIr::build_loop(&chunk, &cfg, head).unwrap();
        let analysis = analyze(&chunk, &ir, &plan.pcs);
        (chunk, ir, analysis)
    }

    #[test]
    fn repeated_root_paths_deduplicate_but_array_elements_do_not_become_roots() {
        let (chunk, _, analysis) = inspect("function f(a,n){for(var i=0;i<n;i++){var c=a[i];c.value=c.value+this.child.value+this.child.value;}}");
        let values: Vec<_> = analysis
            .sites()
            .iter()
            .filter(|s| chunk.jit_name(s.name) == "value")
            .collect();
        assert_eq!(values.len(), 2, "varying c.value must remain unselected");
        assert_eq!(values[0].receiver, values[1].receiver);
        assert!(
            matches!(&analysis.nodes()[values[0].receiver.0],Node::OwnObject{name,..} if &**name=="child")
        );
        assert_eq!(analysis.receiver_at(values[0].pc), Some(values[0].receiver));
        assert!(values.iter().all(|s| s.kind == ReadKind::Own));
    }

    #[test]
    fn identical_phi_inputs_qualify_but_distinct_and_unknown_inputs_do_not() {
        for (other, expected) in [("this.child", 1), ("this.other", 0), ("c", 0)] {
            let source=format!("function f(a,n){{for(var i=0;i<n;i++){{var c=a[i],t;if(i<2)t=this.child;else t={other};c.value=t.value;}}}}");
            let (chunk, _, analysis) = inspect(&source);
            assert_eq!(
                analysis
                    .sites()
                    .iter()
                    .filter(|s| chunk.jit_name(s.name) == "value")
                    .count(),
                expected,
                "{other}"
            );
        }
    }

    #[test]
    fn written_entry_local_is_not_assumed_to_keep_its_initial_object() {
        let (chunk, _, analysis) = inspect(
            "function f(a,root,n){for(var i=0;i<n;i++){var c=a[i];c.value=root.value;root=c;}}",
        );
        assert!(!analysis.nodes().contains(&Node::EntryLocal(1)));
        assert!(!analysis
            .sites()
            .iter()
            .any(|s| chunk.jit_name(s.name) == "value"));
        let (chunk, _, analysis) =
            inspect("function f(a,root,n){for(var i=0;i<n;i++){var c=a[i];c.value=root.value;}}");
        assert!(analysis
            .sites()
            .iter()
            .any(|s| chunk.jit_name(s.name) == "value"));
    }

    #[test]
    fn method_retained_receiver_and_conditional_callee_have_distinct_provenance() {
        let (chunk,ir,analysis)=inspect("function f(a,n){for(var i=0;i<n;i++){var c=a[i];if(i<0)this.method();c.value=this.child.value;}}");
        let inst = ir
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .find(|s| matches!(chunk.jit_ops()[s.pc], Op::GetMethod(..)))
            .unwrap();
        let receiver = analysis
            .value_node(inst.outputs[0])
            .expect("retained receiver");
        let method = analysis
            .value_node(inst.outputs[1])
            .expect("conditional method");
        assert_eq!(analysis.nodes()[receiver.0], Node::This);
        assert!(
            matches!(&analysis.nodes()[method.0],Node::MethodObject{receiver:r,..} if *r==receiver)
        );
        assert_ne!(receiver, method);
    }

    #[test]
    fn profitability_sites_are_bounded_even_with_many_invariant_reads() {
        let reads = (0..40)
            .map(|n| format!("c.value=this.field{n};"))
            .collect::<String>();
        let source = format!("function f(a,n){{for(var i=0;i<n;i++){{var c=a[i];{reads}}}}}");
        let (_, _, analysis) = inspect(&source);
        assert_eq!(analysis.sites().len(), 32);
        assert!(analysis.sites().windows(2).all(|p| p[0].pc < p[1].pc));
    }
}
