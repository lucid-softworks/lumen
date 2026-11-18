//! Lexical POSIX normalization; separators are ASCII '/' on every host platform.
use lumen_host::{Ctx, Value};

fn path_arg<'a>(
    ctx: &mut Ctx,
    args: &'a [Value],
    index: usize,
    name: &str,
) -> Result<&'a str, Value> {
    match args.get(index) {
        Some(Value::Str(s)) => Ok(s),
        _ => {
            let error = ctx.make_error(
                "TypeError",
                format!("The \"{name}\" argument must be of type string"),
            );
            let _ = ctx.set_member(&error, "code", Value::str("ERR_INVALID_ARG_TYPE"));
            Err(error)
        }
    }
}

fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }
    let mut result = String::with_capacity(path.len() + 2);
    if absolute {
        result.push('/');
    }
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            result.push('/');
        }
        result.push_str(part);
    }
    if result.is_empty() {
        result.push('.');
    }
    if trailing && result != "/" {
        result.push('/');
    }
    result
}

pub fn op_normalize(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(Value::from_string(normalize(path_arg(
        ctx, args, 0, "path",
    )?)))
}

pub fn op_join(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mut joined = String::new();
    for index in 0..args.len() {
        let path = path_arg(ctx, args, index, &format!("paths[{index}]"))?;
        if !path.is_empty() {
            if !joined.is_empty() {
                joined.push('/');
            }
            joined.push_str(path);
        }
    }
    Ok(Value::from_string(normalize(&joined)))
}
