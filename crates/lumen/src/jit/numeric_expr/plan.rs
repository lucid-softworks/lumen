//! Infer object and numeric values across an acyclic stack expression ending at a store.
use crate::bytecode::{Chunk, Op};
use crate::jit_ir::Cfg;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rep {
    Object,
    Number,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Source {
    Local(u16),
    This,
    Property {
        receiver: usize,
        name: u32,
        cache: u32,
    },
    Constant(u64),
    Arithmetic {
        left: usize,
        right: usize,
        operation: u32,
    },
    Negate(usize),
}

pub(super) struct Value {
    pub source: Source,
    pub rep: Option<Rep>,
    pub register: u32,
}

pub(super) struct Plan {
    pub end: usize,
    pub values: Vec<Value>,
    pub destination: Option<usize>,
    pub result: usize,
    #[cfg(test)]
    pub prefix_depth: usize,
}

pub(super) fn build(chunk: &Chunk, cfg: &Cfg, start: usize) -> Option<Plan> {
    // Builder's stack is relative to the existing operand prefix. Its checked
    // pops never consume that prefix; successful emission appends terminal operands.
    let prefix_depth = cfg.stack_depth_at(start)?;
    if prefix_depth != 0 && std::env::var_os("LUMEN_JIT_NO_NUMERIC_EXPR_PREFIX").is_some() {
        return None;
    }
    let mut builder = Builder::default();
    for pc in start..(start + 32).min(chunk.jit_ops().len()) {
        let op = chunk.jit_ops()[pc];
        if matches!(
            op,
            Op::SetPropDrop(..)
                | Op::SetPropThisDrop(..)
                | Op::SetPropLocalDrop(..)
                | Op::StoreLocal(_)
                | Op::Return
                | Op::Jump(_)
        ) {
            let plan = builder.finish(pc, matches!(op, Op::SetPropDrop(..)))?;
            #[cfg(test)]
            let plan = Plan {
                prefix_depth,
                ..plan
            };
            return Some(plan);
        }
        builder.step(chunk, op)?;
        if builder.stack.len() > 8 {
            return None;
        }
    }
    None
}

#[derive(Default)]
struct Builder {
    values: Vec<Value>,
    stack: Vec<usize>,
    properties: usize,
    arithmetic: usize,
}

impl Builder {
    fn value(&mut self, source: Source) -> usize {
        let index = self.values.len();
        self.values.push(Value {
            source,
            rep: None,
            register: 0,
        });
        index
    }

    fn root(&mut self, source: Source) -> Option<usize> {
        if matches!(source, Source::Local(slot) if slot as usize * 16 + 16 >= 4096) {
            return None;
        }
        if let Some(index) = self.values.iter().position(|v| match (v.source, source) {
            (Source::Local(a), Source::Local(b)) => a == b,
            (Source::This, Source::This) => true,
            _ => false,
        }) {
            return Some(index);
        }
        Some(self.value(source))
    }

    fn require(&mut self, value: usize, rep: Rep) -> Option<()> {
        let old = &mut self.values.get_mut(value)?.rep;
        if old.is_some_and(|old| old != rep) {
            return None;
        }
        *old = Some(rep);
        Some(())
    }

    fn property(&mut self, receiver: usize, name: u32, cache: u32) -> Option<usize> {
        self.require(receiver, Rep::Object)?;
        self.properties += 1;
        Some(self.value(Source::Property {
            receiver,
            name,
            cache,
        }))
    }

    fn step(&mut self, chunk: &Chunk, op: Op) -> Option<()> {
        let value = match op {
            Op::LoadLocal(slot) => self.root(Source::Local(slot))?,
            Op::LoadThis => self.root(Source::This)?,
            Op::GetPropLocal(slot, name, cache) => {
                let receiver = self.root(Source::Local(slot))?;
                self.property(receiver, name, cache)?
            }
            Op::GetPropThis(name, cache) => {
                let receiver = self.root(Source::This)?;
                self.property(receiver, name, cache)?
            }
            Op::GetProp(name, cache) => {
                let receiver = self.stack.pop()?;
                self.property(receiver, name, cache)?
            }
            Op::Const(index) => {
                let value = self.value(Source::Constant(chunk.jit_const_num(index)?));
                self.require(value, Rep::Number)?;
                value
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => {
                let right = self.stack.pop()?;
                let left = self.stack.pop()?;
                self.require(left, Rep::Number)?;
                self.require(right, Rep::Number)?;
                let operation = match op {
                    Op::Add => 0,
                    Op::Sub => 1,
                    Op::Mul => 2,
                    _ => 3,
                };
                let value = self.value(Source::Arithmetic {
                    left,
                    right,
                    operation,
                });
                self.require(value, Rep::Number)?;
                self.arithmetic += 1;
                value
            }
            Op::Neg => {
                let input = self.stack.pop()?;
                self.require(input, Rep::Number)?;
                let value = self.value(Source::Negate(input));
                self.require(value, Rep::Number)?;
                self.arithmetic += 1;
                value
            }
            _ => return None,
        };
        self.stack.push(value);
        Some(())
    }

    fn finish(mut self, end: usize, needs_destination: bool) -> Option<Plan> {
        let (destination, result) = match (needs_destination, self.stack.as_slice()) {
            (true, &[destination, result]) => (Some(destination), result),
            (false, &[result]) => (None, result),
            _ => return None,
        };
        if let Some(destination) = destination {
            self.require(destination, Rep::Object)?;
        }
        self.require(result, Rep::Number)?;
        // A final store destination still needs its owner: it alone does not save
        // an intermediate clone. Require an object property used by another read.
        let borrowed_property = self.values.iter().enumerate().any(|(index, value)| {
            matches!(value.source, Source::Property { .. })
                && value.rep == Some(Rep::Object)
                && self.values.iter().any(|user| {
                    matches!(user.source, Source::Property { receiver, .. } if receiver == index)
                })
        });
        if self.properties == 0 || self.arithmetic == 0 || !borrowed_property {
            return None;
        }
        let (mut objects, mut numbers) = (0, 16);
        for value in &mut self.values {
            value.register = match value.rep? {
                Rep::Object if objects < 8 => {
                    objects += 1;
                    objects - 1
                }
                Rep::Number if numbers < 32 => {
                    numbers += 1;
                    numbers - 1
                }
                _ => return None,
            };
        }
        Some(Plan {
            end,
            values: self.values,
            destination,
            result,
            #[cfg(test)]
            prefix_depth: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Builder, Rep, Source};

    #[test]
    fn one_root_cannot_be_both_an_object_and_a_number() {
        let mut b = Builder::default();
        let root = b.root(Source::Local(0)).unwrap();
        assert_eq!(b.root(Source::Local(0)), Some(root));
        b.require(root, Rep::Object).unwrap();
        assert!(b.require(root, Rep::Number).is_none());
    }

    #[test]
    fn direct_numeric_fields_remain_with_the_existing_chain_backend() {
        fn expression(nested: bool) -> Builder {
            let mut b = Builder::default();
            let root = b.root(Source::Local(0)).unwrap();
            let receiver = if nested {
                b.property(root, 0, 0).unwrap()
            } else {
                root
            };
            let left = b.property(receiver, 1, 4).unwrap();
            b.require(left, Rep::Number).unwrap();
            let right = b.value(Source::Constant(2f64.to_bits()));
            b.require(right, Rep::Number).unwrap();
            let result = b.value(Source::Arithmetic {
                left,
                right,
                operation: 2,
            });
            b.arithmetic = 1;
            b.stack.push(result);
            b
        }
        assert!(expression(false).finish(4, false).is_none());
        assert!(expression(true).finish(5, false).is_some());
    }

    #[test]
    fn final_destination_alone_is_not_a_borrowed_intermediate() {
        let mut b = Builder::default();
        let root = b.root(Source::Local(0)).unwrap();
        let destination = b.property(root, 0, 0).unwrap();
        let left = b.property(root, 1, 4).unwrap();
        b.require(left, Rep::Number).unwrap();
        let right = b.value(Source::Constant(1f64.to_bits()));
        b.require(right, Rep::Number).unwrap();
        let result = b.value(Source::Arithmetic {
            left,
            right,
            operation: 0,
        });
        b.arithmetic = 1;
        b.stack.extend([destination, result]);
        assert!(b.finish(6, true).is_none());
    }
}
