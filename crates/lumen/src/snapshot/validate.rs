//! Validate deferred bodies without allocating AST nodes. Keep field order and depth accounting
//! identical to the decoder; nested function body boundaries are recorded for later direct access.
use super::{dec_declkind, intern_op, Reader, R};

fn sequence(r: &mut Reader, item: fn(&mut Reader) -> R<()>) -> R<()> {
    let count = r.count()?;
    for _ in 0..count {
        item(r)?;
    }
    Ok(())
}

fn optional(r: &mut Reader, item: fn(&mut Reader) -> R<()>) -> R<()> {
    // Match the decoder's optional-field encoding, including its treatment of non-one tags.
    if r.u8()? == 1 {
        item(r)?;
    }
    Ok(())
}

fn string(r: &mut Reader) -> R<()> {
    r.str_slice().map(|_| ())
}
fn boolean(r: &mut Reader) -> R<()> {
    r.bool().map(|_| ())
}
fn number(r: &mut Reader) -> R<()> {
    r.f64().map(|_| ())
}
fn declaration_kind(r: &mut Reader) -> R<()> {
    dec_declkind(r).map(|_| ())
}
fn operator(r: &mut Reader) -> R<()> {
    intern_op(r.str_slice()?).map(|_| ())
}

pub(super) fn statements(r: &mut Reader) -> R<()> {
    sequence(r, statement)
}

fn statement(r: &mut Reader) -> R<()> {
    r.nested(|r| {
        match r.u8()? {
            0 | 12 => expression(r)?,
            1 => {
                declaration_kind(r)?;
                sequence(r, declaration)?;
            }
            2 => function(r)?,
            3 => optional(r, expression)?,
            4 => {
                expression(r)?;
                statement(r)?;
                optional(r, statement)?;
            }
            5 => statements(r)?,
            6 => {
                expression(r)?;
                statement(r)?;
            }
            7 => {
                statement(r)?;
                expression(r)?;
            }
            8 => {
                optional(r, for_init)?;
                optional(r, expression)?;
                optional(r, expression)?;
                statement(r)?;
            }
            9 => {
                optional(r, declaration_kind)?;
                pattern(r)?;
                expression(r)?;
                boolean(r)?;
                boolean(r)?;
                statement(r)?;
            }
            10 | 11 => optional(r, string)?,
            13 => {
                statements(r)?;
                optional(r, |r| {
                    optional(r, pattern)?;
                    statements(r)
                })?;
                optional(r, statements)?;
            }
            14 => {
                expression(r)?;
                sequence(r, |r| {
                    optional(r, expression)?;
                    statements(r)
                })?;
            }
            15 => {
                string(r)?;
                statement(r)?;
            }
            16 => {
                expression(r)?;
                statement(r)?;
            }
            17 => class(r)?,
            18 | 19 => {}
            20 => {
                string(r)?;
                sequence(r, import_spec)?;
                optional(r, string)?;
            }
            21 => {
                sequence(r, |r| {
                    string(r)?;
                    string(r)
                })?;
                optional(r, string)?;
            }
            22 | 23 => statement(r)?,
            24 => {
                string(r)?;
                optional(r, string)?;
            }
            t => return Err(format!("snapshot: bad Stmt tag {t}")),
        }
        Ok(())
    })
}

fn expression(r: &mut Reader) -> R<()> {
    r.nested(|r| {
        match r.u8()? {
            0 | 4 | 16 | 30 => expression(r)?,
            1 => number(r)?,
            2 => {
                crate::bigint::JsBigInt::parse_radix(r.str_slice()?, 16)
                    .ok_or("snapshot: bad bigint")?;
            }
            3 | 8 => string(r)?,
            5 => boolean(r)?,
            6 | 7 | 9 | 17 | 33 | 34 => {}
            10 => {
                string(r)?;
                string(r)?;
            }
            11 => sequence(r, array_element)?,
            12 => sequence(r, property)?,
            13 => function(r)?,
            14 => class(r)?,
            15 => {
                boolean(r)?;
                optional(r, expression)?;
            }
            18 => {
                operator(r)?;
                expression(r)?;
            }
            19 => {
                operator(r)?;
                boolean(r)?;
                expression(r)?;
            }
            20..=22 => {
                operator(r)?;
                expression(r)?;
                expression(r)?;
            }
            23 => {
                expression(r)?;
                expression(r)?;
                expression(r)?;
            }
            24 => {
                expression(r)?;
                sequence(r, array_element)?;
                boolean(r)?;
            }
            25 => {
                expression(r)?;
                sequence(r, array_element)?;
            }
            26 => {
                expression(r)?;
                string(r)?;
                boolean(r)?;
            }
            27 => {
                expression(r)?;
                expression(r)?;
                boolean(r)?;
            }
            28 => sequence(r, expression)?,
            29 => {
                expression(r)?;
                sequence(r, |r| {
                    optional(r, string)?;
                    string(r)
                })?;
                sequence(r, expression)?;
            }
            31 => {
                string(r)?;
                expression(r)?;
            }
            32 => {
                expression(r)?;
                let phase = r.u8()?;
                if phase > 2 {
                    return Err(format!("snapshot: bad ImportPhase {phase}"));
                }
                optional(r, expression)?;
            }
            t => return Err(format!("snapshot: bad Expr tag {t}")),
        }
        Ok(())
    })
}

fn array_element(r: &mut Reader) -> R<()> {
    match r.u8()? {
        0 | 1 => expression(r),
        2 => Ok(()),
        t => Err(format!("snapshot: bad ArrayElem {t}")),
    }
}

fn property(r: &mut Reader) -> R<()> {
    match r.u8()? {
        0 | 1 => {
            property_key(r)?;
            expression(r)
        }
        2..=4 => {
            property_key(r)?;
            function(r)
        }
        5 | 6 => expression(r),
        t => Err(format!("snapshot: bad PropDef {t}")),
    }
}

fn property_key(r: &mut Reader) -> R<()> {
    match r.u8()? {
        0 | 1 => string(r),
        2 => number(r),
        3 => expression(r),
        t => Err(format!("snapshot: bad PropKey {t}")),
    }
}

fn pattern(r: &mut Reader) -> R<()> {
    r.nested(|r| match r.u8()? {
        0 => string(r),
        1 => sequence(r, |r| match r.u8()? {
            0 => Ok(()),
            1 => {
                pattern(r)?;
                optional(r, expression)
            }
            2 => pattern(r),
            t => Err(format!("snapshot: bad ArrayPatElem {t}")),
        }),
        2 => {
            sequence(r, |r| {
                property_key(r)?;
                pattern(r)?;
                optional(r, expression)
            })?;
            optional(r, string)
        }
        3 => expression(r),
        t => Err(format!("snapshot: bad Pattern {t}")),
    })
}

fn function(r: &mut Reader) -> R<()> {
    r.nested(|r| {
        optional(r, string)?;
        sequence(r, |r| {
            pattern(r)?;
            optional(r, expression)?;
            boolean(r)
        })?;
        let start = r.pos;
        statements(r)?;
        r.record_body(start);
        r.u8()?; // Packed flags; the decoder ignores unused bits.
        super::dec_opt_source(r).map(|_| ())
    })
}

fn class(r: &mut Reader) -> R<()> {
    optional(r, string)?;
    optional(r, expression)?;
    sequence(r, |r| {
        property_key(r)?;
        let kind = r.u8()?;
        if kind > 6 {
            return Err(format!("snapshot: bad MemberKind {kind}"));
        }
        boolean(r)?;
        optional(r, function)?;
        optional(r, expression)?;
        sequence(r, expression)
    })?;
    sequence(r, expression)?;
    super::dec_opt_source(r).map(|_| ())
}

fn declaration(r: &mut Reader) -> R<()> {
    pattern(r)?;
    optional(r, expression)
}

fn for_init(r: &mut Reader) -> R<()> {
    match r.u8()? {
        0 => {
            declaration_kind(r)?;
            sequence(r, declaration)
        }
        1 => expression(r),
        t => Err(format!("snapshot: bad ForInit {t}")),
    }
}

fn import_spec(r: &mut Reader) -> R<()> {
    match r.u8()? {
        0..=3 => string(r),
        4 => {
            string(r)?;
            string(r)
        }
        t => Err(format!("snapshot: bad ImportSpec {t}")),
    }
}
