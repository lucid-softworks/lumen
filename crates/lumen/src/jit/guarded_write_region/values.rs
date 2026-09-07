//! Typed, borrowed values for a read-only expression inside a guarded write region.
use crate::bytecode::{Chunk, Op};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Rep {
    Object,
    Number,
}

#[derive(Clone, Copy)]
pub(super) enum Source {
    Local(u16),
    This,
    Name(u32, u32),
    Constant(u64),
    Property {
        receiver: usize,
        name: u32,
        cache: u32,
    },
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

pub(super) struct Expression {
    pub values: Vec<Value>,
    pub outputs: Vec<usize>,
}

#[derive(Default)]
pub(super) struct Builder {
    values: Vec<Value>,
    stack: Vec<usize>,
    name_floor: usize,
}

impl Builder {
    fn value(&mut self, source: Source) -> usize {
        let id = self.values.len();
        self.values.push(Value {
            source,
            rep: None,
            register: 0,
        });
        id
    }

    fn root(&mut self, source: Source) -> Option<usize> {
        if matches!(source, Source::Local(s) if s as usize * 16 + 16 >= 4096) {
            return None;
        }
        if let Some(id) =
            self.values
                .iter()
                .enumerate()
                .position(|(id, v)| match (v.source, source) {
                    (Source::Local(a), Source::Local(b)) => a == b,
                    (Source::This, Source::This) => true,
                    (Source::Name(a, c), Source::Name(b, d)) => {
                        id >= self.name_floor && a == b && c == d
                    }
                    _ => false,
                })
        {
            return Some(id);
        }
        Some(self.value(source))
    }

    fn require(&mut self, id: usize, rep: Rep) -> Option<()> {
        let old = &mut self.values.get_mut(id)?.rep;
        if old.is_some_and(|old| old != rep) {
            return None;
        }
        *old = Some(rep);
        Some(())
    }

    fn property(&mut self, receiver: usize, name: u32, cache: u32) -> Option<usize> {
        self.require(receiver, Rep::Object)?;
        Some(self.value(Source::Property {
            receiver,
            name,
            cache,
        }))
    }

    pub(super) fn step(&mut self, chunk: &Chunk, op: Op) -> Option<()> {
        let id = match op {
            Op::LoadLocal(s) => self.root(Source::Local(s))?,
            Op::LoadThis => self.root(Source::This)?,
            Op::LoadName(n, c) => self.root(Source::Name(n, c))?,
            Op::GetPropLocal(s, n, c) => {
                let receiver = self.root(Source::Local(s))?;
                self.property(receiver, n, c)?
            }
            Op::GetPropThis(n, c) => {
                let receiver = self.root(Source::This)?;
                self.property(receiver, n, c)?
            }
            Op::GetProp(n, c) => {
                let receiver = self.stack.pop()?;
                self.property(receiver, n, c)?
            }
            Op::Const(k) => {
                let id = self.value(Source::Constant(chunk.jit_const_num(k)?));
                self.require(id, Rep::Number)?;
                id
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
                let id = self.value(Source::Arithmetic {
                    left,
                    right,
                    operation,
                });
                self.require(id, Rep::Number)?;
                id
            }
            Op::Neg => {
                let input = self.stack.pop()?;
                self.require(input, Rep::Number)?;
                let id = self.value(Source::Negate(input));
                self.require(id, Rep::Number)?;
                id
            }
            _ => return None,
        };
        self.stack.push(id);
        (self.stack.len() <= 8 && self.values.len() <= 32).then_some(())
    }

    pub(super) fn snapshot(&self) -> Vec<usize> {
        self.stack.clone()
    }

    pub(super) fn value_count(&self) -> usize {
        self.values.len()
    }

    pub(super) fn sequence_store(&mut self, op: Op) -> Option<(usize, usize)> {
        let implicit = match op {
            Op::SetPropThisDrop(..) => Some(self.root(Source::This)?),
            Op::SetPropLocalDrop(s, ..) => Some(self.root(Source::Local(s))?),
            Op::SetProp(..) | Op::SetPropDrop(..) => None,
            _ => return None,
        };
        let number = self.stack.pop()?;
        let receiver = match implicit {
            Some(root) => root,
            None => self.stack.pop()?,
        };
        self.require(receiver, Rep::Object)?;
        self.require(number, Rep::Number)?;
        if matches!(op, Op::SetProp(..)) {
            self.stack.push(number);
        }
        // A numeric store can alias a global binding. Future name reads must be live;
        // unchanged physical locals/this remain safe roots, and properties are never CSE'd.
        self.name_floor = self.values.len();
        Some((receiver, number))
    }

    pub(super) fn sequence_finish(self) -> Option<Expression> {
        self.finish(&[])
    }

    pub(super) fn comparison(self) -> Option<Expression> {
        self.finish(&[Rep::Number, Rep::Number])
    }

    pub(super) fn store(mut self, op: Op) -> Option<Expression> {
        let root = match op {
            Op::SetPropThisDrop(..) => Some(self.root(Source::This)?),
            Op::SetPropLocalDrop(s, ..) => Some(self.root(Source::Local(s))?),
            Op::SetPropDrop(..) => None,
            _ => return None,
        };
        if let Some(root) = root {
            self.stack.insert(0, root);
        }
        self.finish(&[Rep::Object, Rep::Number])
    }

    fn finish(mut self, reps: &[Rep]) -> Option<Expression> {
        if self.stack.len() != reps.len() {
            return None;
        }
        for (id, rep) in self.stack.clone().into_iter().zip(reps) {
            self.require(id, *rep)?;
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
        Some(Expression {
            values: self.values,
            outputs: self.stack,
        })
    }
}
