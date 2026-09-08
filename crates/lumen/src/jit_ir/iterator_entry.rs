//! Proof planning only: a Candidate is NOT permission to execute a native trace.
//! Every runtime obligation must be discharged before any deferred store commits.
use crate::bytecode::Chunk;

mod plan;
use plan::analyze_ops;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Literal {
    Number(u64),
    Boolean(bool),
    Undefined,
    Null,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Constant(Literal),
    Name {
        name: String,
        cache: u32,
    },
    This,
    Own {
        receiver: usize,
        name: String,
        cache: u32,
    },
    Dense {
        receiver: usize,
        index: usize,
    },
    Binary {
        op: Binary,
        left: usize,
        right: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Binary {
    Add,
    Sub,
    Mul,
    Div,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Obligation {
    /// Caller must prove normal zero-argument callee entry with no observable
    /// activation setup, arguments object, eval, closure initialization or handler.
    CalleeEntryAndEnvironment,
    NormalCallDepthAndGcSafepointBeforeProbes,
    /// Every guard miss resumes the original callee entry before any mutation.
    AllGuardsAndYieldOwnerBeforeCommit,
    NameResolution {
        value: usize,
    },
    /// Guarded ordinary own data OR own Array length/data; never a prototype getter.
    OwnData {
        value: usize,
    },
    /// Both Binary operands must be Number; comparison results are Boolean.
    BinaryNumericOperands {
        expression: usize,
    },
    /// Store RHS itself must be Number.
    NumberValue {
        value: usize,
    },
    /// Runtime truthiness must preserve HTMLDDA; a Boolean result is sufficient.
    Truthiness {
        value: usize,
    },
    WritableOrdinaryNonIndexOldNumber {
        store_pc: usize,
    },
    /// Resolve actual storage addresses. Forward the pending Number on equality;
    /// otherwise prove disjointness. Name spelling alone proves neither outcome.
    ForwardStoredEntryOrProveDisjoint {
        read: usize,
        store_pc: usize,
    },
    /// Dense reads must be present own data on a real Array, exact bounded index.
    /// Ordinary numeric field store must not alias/mutate its indexed storage.
    DenseArrayDisjointFromStore {
        read: usize,
        store_pc: Option<usize>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Branch {
    pub pc: usize,
    pub condition: usize,
    pub required_truthy: bool,
    pub cold_target: usize,
    pub retains_condition: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DeferredStore {
    pub pc: usize,
    pub receiver: usize,
    pub name: String,
    pub cache: u32,
    pub value: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub expressions: Vec<Expr>,
    pub branches: Vec<Branch>,
    pub store: Option<DeferredStore>,
    pub obligations: Vec<Obligation>,
    pub done: usize,
    pub value: usize,
    pub return_pc: usize,
    pub tdz_locals: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reject {
    pub pc: usize,
    pub reason: &'static str,
}

pub(crate) fn analyze(chunk: &Chunk) -> Result<Candidate, Reject> {
    analyze_ops(
        chunk.jit_ops(),
        &|n| Some(chunk.jit_name(n).to_owned()),
        &|n| {
            if !chunk.jit_const_copyable(n) {
                return None;
            }
            let (tag, bits) = chunk.jit_const_bits(n);
            match tag & 255 {
                0 => Some(Literal::Undefined),
                2 => Some(Literal::Null),
                3 => Some(Literal::Boolean(tag >> 8 != 0)),
                4 => Some(Literal::Number(bits)),
                _ => None,
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn renamed_compiled_iterator_entry_retains_runtime_proof_requirements() {
        let source = r#"function next(){while(state.position<ceiling){
        if(state.items.length>0&&state.cursor<state.items.length){
            state.cursor=state.cursor+1;
            return {value:state.items[state.cursor-1],done:false};
        } cold();}return {value:undefined,done:true};}"#;
        let statements =
            crate::parser::parse_script(source, false).unwrap_or_else(|_| panic!("valid fixture"));
        let crate::ast::Stmt::FuncDecl(function) = &statements[0] else {
            panic!("function")
        };
        let chunk = crate::bytecode::compile(function).expect("compiled iterator");
        let plan = analyze(&chunk).expect("bounded entry prefix");
        assert!(plan.return_pc < 64);
        assert!(plan.store.is_some());
        assert!(plan
            .obligations
            .iter()
            .any(|o| matches!(o, Obligation::ForwardStoredEntryOrProveDisjoint { .. })));
        assert!(plan.obligations.iter().any(|o| matches!(
            o,
            Obligation::DenseArrayDisjointFromStore {
                store_pc: Some(_),
                ..
            }
        )));
    }
}
