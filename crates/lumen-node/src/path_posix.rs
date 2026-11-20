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

pub fn op_resolve(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    // Walk backwards so an absolute argument stops both resolution and validation of earlier
    // arguments. Keep borrowed segments until the final join instead of repeatedly prepending.
    let mut parts = Vec::new();
    let mut absolute = false;
    for index in (0..args.len()).rev() {
        let path = path_arg(ctx, args, index, &format!("paths[{index}]"))?;
        if !path.is_empty() {
            parts.push(path);
            if path.starts_with('/') {
                absolute = true;
                break;
            }
        }
    }
    let cwd;
    if !absolute {
        // Node observes process.cwd() at call time, including user replacements and throws.
        let global = ctx.global_this();
        let process = ctx.member_get(&global, "process")?;
        let get_cwd = ctx.member_get(&process, "cwd")?;
        let value = ctx.invoke(get_cwd, process, &[])?;
        let Value::Str(value) = value else {
            return Err(ctx.make_error("TypeError", "process.cwd() must return a string"));
        };
        #[cfg(windows)]
        {
            let posix = value.replace('\\', "/");
            cwd = posix[posix.find('/').unwrap_or(0)..].to_owned();
        }
        #[cfg(not(windows))]
        {
            cwd = value.to_string();
        }
        if cwd.starts_with('/')
            && (args.is_empty()
                || (args.len() == 1
                    && matches!(&args[0], Value::Str(s) if s.is_empty() || &**s == ".")))
        {
            return Ok(Value::str(&cwd));
        }
        parts.push(&cwd);
    }
    parts.reverse();
    let normalized = normalize(&parts.join("/"));
    let resolved = if normalized == "/" {
        "/"
    } else {
        normalized.trim_end_matches('/')
    };
    Ok(Value::str(resolved))
}

pub fn op_dirname(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let path = path_arg(ctx, args, 0, "path")?;
    let trimmed = path.trim_end_matches('/');
    let dirname = match trimmed.rfind('/') {
        Some(0) => "/",
        Some(1) if path.starts_with('/') => "//",
        Some(end) => &path[..end],
        None if path.starts_with('/') => "/",
        None => ".",
    };
    Ok(Value::str(dirname))
}
