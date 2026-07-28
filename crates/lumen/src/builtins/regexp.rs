//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

/// A RegExp prototype getter: a flag boolean (`Some(char)`), or the special source/flags string.
fn re_flag_get(i: &Interp, this: &Value, flag: Option<char>) -> Result<Value, Value> {
    if let Some(ptr) = map_ptr(this) {
        if let Some(re) = i.regexps.get(&ptr) {
            return Ok(match flag {
                Some(c) => Value::Bool(re.flags.contains(c)),
                None => Value::from_string(re.flags.clone()),
            });
        }
        // The %RegExp.prototype% object itself has default values rather than throwing.
        if i.extra_protos.get("RegExp").map(|p| Rc::as_ptr(p) as usize) == Some(ptr) {
            return Ok(match flag {
                Some(_) => Value::Undefined,
                None => Value::str(""),
            });
        }
    }
    Err(i.make_error(
        "TypeError",
        "RegExp.prototype getter called on a non-RegExp",
    ))
}
fn re_source_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    if let Some(ptr) = map_ptr(&this) {
        if let Some(re) = i.regexps.get(&ptr) {
            if re.source.is_empty() {
                return Ok(Value::str("(?:)"));
            }
            // EscapeRegExpPattern: "/" and line terminators are escaped so "/"+S+"/"+F re-parses.
            let mut out = String::new();
            let mut escaped = false;
            let mut in_class = false;
            for c in re.source.chars() {
                // A line terminator renders as its escape sequence; directly after a backslash the
                // pair `\<LF>` collapses to `\n` (both match the terminator), reusing that slash.
                let terminator = match c {
                    '\n' => Some("n"),
                    '\r' => Some("r"),
                    '\u{2028}' => Some("u2028"),
                    '\u{2029}' => Some("u2029"),
                    _ => None,
                };
                if let Some(esc) = terminator {
                    if !escaped {
                        out.push('\\');
                    }
                    out.push_str(esc);
                    escaped = false;
                    continue;
                }
                if escaped {
                    out.push(c);
                    escaped = false;
                    continue;
                }
                match c {
                    '\\' => {
                        out.push(c);
                        escaped = true;
                    }
                    '[' => {
                        in_class = true;
                        out.push(c);
                    }
                    ']' => {
                        in_class = false;
                        out.push(c);
                    }
                    // Inside a character class a solidus can't terminate the literal.
                    '/' if !in_class => out.push_str("\\/"),
                    c => out.push(c),
                }
            }
            return Ok(Value::from_string(out));
        }
        if i.extra_protos.get("RegExp").map(|p| Rc::as_ptr(p) as usize) == Some(ptr) {
            return Ok(Value::str("(?:)"));
        }
    }
    Err(i.make_error(
        "TypeError",
        "RegExp.prototype.source called on a non-RegExp",
    ))
}

fn re_flags_get(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    if !matches!(this, Value::Obj(_)) {
        return Err(i.make_error(
            "TypeError",
            "RegExp.prototype.flags getter called on non-object",
        ));
    }
    let mut out = String::new();
    for (prop, ch) in [
        ("hasIndices", 'd'),
        ("global", 'g'),
        ("ignoreCase", 'i'),
        ("multiline", 'm'),
        ("dotAll", 's'),
        ("unicode", 'u'),
        ("unicodeSets", 'v'),
        ("sticky", 'y'),
    ] {
        let v = ab(i.get_member(&this, prop))?;
        if i.to_boolean(&v) {
            out.push(ch);
        }
    }
    Ok(Value::from_string(out))
}

macro_rules! regexp_flag_getter {
    ($name:ident, $flag:literal) => {
        fn $name(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
            re_flag_get(i, &this, Some($flag))
        }
    };
}
regexp_flag_getter!(re_has_indices_get, 'd');
regexp_flag_getter!(re_global_get, 'g');
regexp_flag_getter!(re_ignore_case_get, 'i');
regexp_flag_getter!(re_multiline_get, 'm');
regexp_flag_getter!(re_dot_all_get, 's');
regexp_flag_getter!(re_unicode_get, 'u');
regexp_flag_getter!(re_unicode_sets_get, 'v');
regexp_flag_getter!(re_sticky_get, 'y');

/// The native symbol methods may read `flags`, every component flag getter, and `exec`.
/// A wrapper-eliding specialization must prove those live inherited dependencies, not merely
/// the outer symbol-method identity.
pub(super) fn literal_match_dependencies_canonical(i: &Interp) -> bool {
    let Some(proto) = i.extra_protos.get("RegExp") else {
        return false;
    };
    let p = proto.borrow();
    let names = [
        "flags",
        "hasIndices",
        "global",
        "ignoreCase",
        "multiline",
        "dotAll",
        "unicode",
        "unicodeSets",
        "sticky",
        "exec",
    ];
    let proto_ptr = Rc::as_ptr(proto) as usize;
    let shape = p.props.shape();
    let cached = i.regexp_dependency_cache.get();
    let slots = if cached.proto == proto_ptr && cached.shape == shape {
        cached.slots
    } else {
        let mut slots = [u32::MAX; 10];
        for (index, name) in names.iter().enumerate() {
            let Some(slot) = p.props.slot_of(name) else {
                return false;
            };
            slots[index] = slot as u32;
        }
        i.regexp_dependency_cache
            .set(crate::interpreter::RegexpDependencyCache {
                proto: proto_ptr,
                shape,
                slots,
            });
        slots
    };
    let property_at = |index: usize| {
        p.props
            .entry_at(slots[index] as usize)
            .filter(|(key, _)| &**key == names[index])
            .map(|(_, property)| property)
    };
    let getter_is = |index: usize, native: NativeFn| {
        property_at(index).is_some_and(|property| {
            matches!(
                property.getter(),
                Some(Value::Obj(function))
                    if matches!(
                        function.borrow().call,
                        Callable::Native(found)
                            if found as usize == native as *const () as usize
                    )
            )
        })
    };
    getter_is(0, re_flags_get)
        && getter_is(1, re_has_indices_get)
        && getter_is(2, re_global_get)
        && getter_is(3, re_ignore_case_get)
        && getter_is(4, re_multiline_get)
        && getter_is(5, re_dot_all_get)
        && getter_is(6, re_unicode_get)
        && getter_is(7, re_unicode_sets_get)
        && getter_is(8, re_sticky_get)
        && property_at(9)
            .filter(|property| !property.accessor())
            .map(Property::value)
            .is_some_and(|value| {
                matches!(
                    value,
                    Value::Obj(ref function)
                        if matches!(
                            function.borrow().call,
                            Callable::Native(native)
                                if native as usize == regexp_exec as *const () as usize
                        )
                )
            })
}

/// The second (flags) argument to RegExp / RegExp.prototype.compile: undefined → "", else ToString.
fn regexp_flags_arg(i: &mut Interp, a: &[Value]) -> Result<String, Value> {
    match arg(a, 1) {
        Value::Undefined => Ok(String::new()),
        v => Ok(ab(i.to_string(&v))?.to_string()),
    }
}

pub(super) fn install_regexp(it: &mut Interp) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert("RegExp", proto.clone());
    // source/flags/global/... accessor getters (computed from the matcher).
    let add_getter = |it: &mut Interp, proto: &Gc, name: &str, f: NativeFn| {
        let g = it.make_native(&format!("get {name}"), 0, f);
        proto.borrow_mut().props.insert(
            name,
            Property::accessor_prop(Some(Value::Obj(g)), None, false, true),
        );
    };
    add_getter(it, &proto, "source", re_source_get);
    // `get flags` is generic: it reads each component flag accessor via [[Get]] on the receiver.
    add_getter(it, &proto, "flags", re_flags_get);
    add_getter(it, &proto, "global", re_global_get);
    add_getter(it, &proto, "ignoreCase", re_ignore_case_get);
    add_getter(it, &proto, "multiline", re_multiline_get);
    add_getter(it, &proto, "dotAll", re_dot_all_get);
    add_getter(it, &proto, "sticky", re_sticky_get);
    add_getter(it, &proto, "unicode", re_unicode_get);
    add_getter(it, &proto, "hasIndices", re_has_indices_get);
    add_getter(it, &proto, "unicodeSets", re_unicode_sets_get);
    // Annex B B.2.5 RegExp.prototype.compile(pattern, flags): recompile this regex in place.
    it.def_method(&proto, "compile", 2, |i, this, a| {
        let ptr = map_ptr(&this).filter(|p| i.regexps.contains_key(p));
        let ptr = ptr.ok_or_else(|| i.make_error("TypeError", "compile called on non-RegExp"))?;
        // The legacy-regexp proposal: compile requires a DIRECT same-realm RegExp instance (a
        // subclass or cross-realm instance is a TypeError).
        if let (Value::Obj(o), Some(rp)) = (&this, i.extra_protos.get("RegExp")) {
            let direct = o
                .borrow()
                .proto
                .as_ref()
                .map(|p| Rc::ptr_eq(p, rp))
                .unwrap_or(false);
            if !direct {
                return Err(i.make_error(
                    "TypeError",
                    "RegExp.prototype.compile requires a direct RegExp instance",
                ));
            }
        }
        let (source, flags) = match arg(a, 0) {
            Value::Obj(o) if i.regexps.contains_key(&(Rc::as_ptr(&o) as usize)) => {
                // A RegExp pattern copies its source/flags; a second flags argument is then an error.
                if !matches!(arg(a, 1), Value::Undefined) {
                    return Err(
                        i.make_error("TypeError", "cannot supply flags when compiling a RegExp")
                    );
                }
                let re = i.regexps[&(Rc::as_ptr(&o) as usize)].clone();
                (re.source.clone(), re.flags.clone())
            }
            Value::Undefined => (String::new(), regexp_flags_arg(i, a)?),
            v => {
                let src = ab(i.to_string(&v))?.to_string();
                (src, regexp_flags_arg(i, a)?)
            }
        };
        let re = crate::regex::Regex::new(&source, &flags)
            .map_err(|e| i.make_error("SyntaxError", &e))?;
        if let Value::Obj(o) = &this {
            i.gc_pin(o);
        }
        i.regexps.insert(ptr, Rc::new(re));
        set_throw(i, &this, "lastIndex", Value::Num(0.0))?;
        ab(i.set_member(&this, "lastIndex", Value::Num(0.0)))?;
        Ok(this)
    });
    it.def_method(&proto, "exec", 1, regexp_exec);
    it.def_method(&proto, "test", 1, |i, this, a| {
        // RegExpExec: an own/inherited callable `exec` takes precedence over the built-in matcher.
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "RegExp.prototype.test requires an object"));
        }
        let s = ab(i.to_string(&arg(a, 0)))?;
        Ok(Value::Bool(!matches!(
            regexp_exec_abstract(i, &this, s)?,
            Value::Null
        )))
    });
    it.def_method(&proto, "toString", 0, |i, this, _| {
        if !matches!(this, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "RegExp.prototype.toString requires an object"));
        }
        let src_v = ab(i.get_member(&this, "source"))?;
        let src = ab(i.to_string(&src_v))?;
        let flags_v = ab(i.get_member(&this, "flags"))?;
        let flags = ab(i.to_string(&flags_v))?;
        Ok(Value::from_string(format!("/{src}/{flags}")))
    });
    let ctor = it.make_native("RegExp", 2, |i, _t, a| {
        let pattern = arg(a, 0);
        let flags_arg = arg(a, 1);
        // IsRegExp: an Object whose truthy @@match (when defined) or [[RegExpMatcher]] slot.
        let has_slots = matches!(&pattern, Value::Obj(o)
            if i.regexps.contains_key(&(Rc::as_ptr(o) as usize)));
        let pattern_is_regexp = if matches!(pattern, Value::Obj(_)) {
            let m = match well_known_key(i, "match") {
                Some(key) => ab(i.get_member(&pattern, &key))?,
                None => Value::Undefined,
            };
            if !matches!(m, Value::Undefined) {
                i.to_boolean(&m)
            } else {
                has_slots
            }
        } else {
            false
        };
        // Called as a function on a regexp(-like) whose .constructor is this same
        // constructor: return the argument unchanged.
        if !i.constructing && pattern_is_regexp && matches!(flags_arg, Value::Undefined) {
            let c = ab(i.get_member(&pattern, "constructor"))?;
            let same = match (&c, i.extra_protos.get("%RegExpCtor%")) {
                (Value::Obj(co), Some(rc)) => Rc::ptr_eq(co, rc),
                _ => false,
            };
            if same {
                return Ok(pattern);
            }
        }
        // Observable step order: the pattern's internal slots (or `source`/`flags` Gets) are
        // read first, then RegExpAlloc fetches newTarget's `prototype`, and only then does
        // RegExpInitialize run the ToString coercions.
        let (src_v, fl_v) = if has_slots {
            let re = match &pattern {
                Value::Obj(o) => i.regexps[&(Rc::as_ptr(o) as usize)].clone(),
                _ => unreachable!(),
            };
            let fl = match flags_arg {
                Value::Undefined => Value::from_string(re.flags.clone()),
                v => v,
            };
            (Value::from_string(re.source.clone()), fl)
        } else if pattern_is_regexp {
            // A regexp-like object: read its `source` and `flags` properties.
            let src = ab(i.get_member(&pattern, "source"))?;
            let fl = match flags_arg {
                Value::Undefined => ab(i.get_member(&pattern, "flags"))?,
                v => v,
            };
            (src, fl)
        } else {
            (pattern, flags_arg)
        };
        let alloc_proto = ab(i.regexp_alloc_proto())?;
        let source = match src_v {
            Value::Undefined => String::new(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        let flags = match fl_v {
            Value::Undefined => String::new(),
            v => ab(i.to_string(&v))?.to_string(),
        };
        ab(i.make_regexp_with_proto(&source, &flags, alloc_proto))
    });
    it.extra_protos.insert("%RegExpCtor%", ctor.clone());
    install_regexp_legacy_statics(it, &ctor);
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    install_species(it, &ctor);

    // The @@match/@@replace/@@search/@@split/@@matchAll methods on RegExp.prototype (what
    // String.prototype.{match,replace,...} dispatch to).
    let methods: [(&str, usize, NativeFn); 5] = [
        ("match", 1, re_sym_match),
        ("replace", 2, re_sym_replace),
        ("search", 1, re_sym_search),
        ("split", 2, re_sym_split),
        ("matchAll", 1, re_sym_matchall),
    ];
    for (sym, len, f) in methods {
        if let Some(key) = well_known_key(it, sym) {
            let m = it.make_native(&format!("[Symbol.{sym}]"), len, f);
            proto
                .borrow_mut()
                .props
                .insert(key, Property::builtin(Value::Obj(m)));
        }
    }
    it.def_method(&ctor, "escape", 1, |i, _t, a| {
        let s = match arg(a, 0) {
            Value::Str(s) => s,
            _ => return Err(i.make_error("TypeError", "RegExp.escape requires a string")),
        };
        let mut out = String::new();
        for (idx, cp) in crate::jstr::code_points(&s).into_iter().enumerate() {
            out.push_str(&regexp_escape_cp(cp, idx == 0));
        }
        Ok(Value::from_string(out))
    });
    set_builtin(&it.global, "RegExp", Value::Obj(ctor));
}

/// EncodeForRegExpEscape: escape one code point for `RegExp.escape` (`cp` may be a lone
/// surrogate, which always hex-escapes).
fn regexp_escape_cp(cp: u32, first: bool) -> String {
    if (0xD800..0xE000).contains(&cp) {
        return format!("\\u{cp:04x}");
    }
    let c = char::from_u32(cp).unwrap_or('\u{FFFD}');
    // The first character, if alphanumeric, is hex-escaped so the result can't start an identifier.
    if first && c.is_ascii_alphanumeric() {
        return format!("\\x{cp:02x}");
    }
    if "^$\\.*+?()[]{}|/".contains(c) {
        return format!("\\{c}");
    }
    match c {
        '\t' => "\\t".into(),
        '\n' => "\\n".into(),
        '\u{0b}' => "\\v".into(),
        '\u{0c}' => "\\f".into(),
        '\r' => "\\r".into(),
        _ if ",-=<>#&!%:;@~'`\"".contains(c)
            || c.is_whitespace()
            || c == '\u{FEFF}'
            || c.is_control() =>
        {
            if cp <= 0xff {
                format!("\\x{cp:02x}")
            } else if cp <= 0xffff {
                format!("\\u{cp:04x}")
            } else {
                format!("\\u{{{cp:x}}}")
            }
        }
        _ => c.to_string(),
    }
}

/// The legacy RegExp static accessors (`RegExp.input`/`$_`, `lastMatch`/`$&`, `lastParen`/`$+`,
/// `leftContext`/`` $` ``, `rightContext`/`$'`, `$1`..`$9`): getters (and the input setter)
/// brand-check that the receiver IS this realm's %RegExp% constructor.
fn regexp_legacy_brand(i: &mut Interp, this: &Value) -> Result<Gc, Value> {
    let ctor = i.extra_protos.get("%RegExpCtor%").cloned();
    match (this, ctor) {
        (Value::Obj(o), Some(c)) if Rc::ptr_eq(o, &c) => Ok(c),
        _ => Err(i.make_error(
            "TypeError",
            "RegExp legacy static accessor called on an incompatible receiver",
        )),
    }
}

fn regexp_legacy_get(i: &mut Interp, this: &Value, slot: &str) -> Result<Value, Value> {
    let c = regexp_legacy_brand(i, this)?;
    // Materialize the deferred last-match state (if any) before reading.
    super::flush_regexp_legacy(i);
    let v = c.borrow().props.get(slot).map(|p| p.value());
    Ok(v.unwrap_or_else(|| Value::str("")))
}

macro_rules! legacy_getter {
    ($fname:ident, $slot:literal) => {
        fn $fname(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
            regexp_legacy_get(i, &this, $slot)
        }
    };
}
legacy_getter!(lg_input, "__legacy_input");
legacy_getter!(lg_last_match, "__legacy_lastMatch");
legacy_getter!(lg_last_paren, "__legacy_lastParen");
legacy_getter!(lg_left, "__legacy_leftContext");
legacy_getter!(lg_right, "__legacy_rightContext");
legacy_getter!(lg_d1, "__legacy_$1");
legacy_getter!(lg_d2, "__legacy_$2");
legacy_getter!(lg_d3, "__legacy_$3");
legacy_getter!(lg_d4, "__legacy_$4");
legacy_getter!(lg_d5, "__legacy_$5");
legacy_getter!(lg_d6, "__legacy_$6");
legacy_getter!(lg_d7, "__legacy_$7");
legacy_getter!(lg_d8, "__legacy_$8");
legacy_getter!(lg_d9, "__legacy_$9");

fn lg_set_input(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    regexp_legacy_brand(i, &this)?;
    let v = ab(i.to_string(&arg(a, 0)))?;
    let c = regexp_legacy_brand(i, &this)?;
    // Materialize any deferred match first so this write isn't later clobbered by its flush.
    super::flush_regexp_legacy(i);
    c.borrow_mut().props.insert(
        "__legacy_input",
        Property::data(Value::Str(v), true, false, false),
    );
    Ok(Value::Undefined)
}

pub(super) fn install_regexp_legacy_statics(it: &mut Interp, ctor: &Gc) {
    let entries: [(&[&str], NativeFn, bool); 14] = [
        (&["input", "$_"], lg_input, true),
        (&["lastMatch", "$&"], lg_last_match, false),
        (&["lastParen", "$+"], lg_last_paren, false),
        (&["leftContext", "$`"], lg_left, false),
        (&["rightContext", "$'"], lg_right, false),
        (&["$1"], lg_d1, false),
        (&["$2"], lg_d2, false),
        (&["$3"], lg_d3, false),
        (&["$4"], lg_d4, false),
        (&["$5"], lg_d5, false),
        (&["$6"], lg_d6, false),
        (&["$7"], lg_d7, false),
        (&["$8"], lg_d8, false),
        (&["$9"], lg_d9, false),
    ];
    for (names, get, with_set) in entries {
        for name in names {
            let g = it.make_native("get", 0, get);
            let set = if with_set {
                Some(Value::Obj(it.make_native("set", 1, lg_set_input)))
            } else {
                None
            };
            ctor.borrow_mut().props.insert(
                *name,
                Property::accessor_prop(Some(Value::Obj(g)), set, false, true),
            );
        }
    }
}

/// `this` must be an Object for the generic `RegExp.prototype[@@…]` methods (they read flags and
/// call `exec` through ordinary property access rather than the internal slots).
fn require_regexp_this(i: &mut Interp, this: &Value, name: &str) -> Result<(), Value> {
    match this {
        Value::Obj(_) => Ok(()),
        _ => Err(i.make_error("TypeError", format!("{name} called on a non-object"))),
    }
}

pub(super) fn re_sym_match(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.match]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let flags = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags))?;
    if !flags.contains('g') {
        return regexp_exec_abstract(i, &this, s);
    }
    let unicode = flags.contains('u') || flags.contains('v');
    set_throw(i, &this, "lastIndex", Value::Num(0.0))?;
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_abstract(i, &this, s.clone())?;
        if matches!(result, Value::Null) {
            break;
        }
        let m0 = ab(i.get_member(&result, "0"))?;
        let match_str = ab(i.to_string(&m0))?;
        results.push(Value::Str(match_str.clone()));
        if match_str.is_empty() {
            let li = ab(i.get_member(&this, "lastIndex"))?;
            let li = to_length_val(i, &li)?;
            let next = advance_string_index(li, &s, unicode);
            set_throw(i, &this, "lastIndex", Value::Num(next as f64))?;
        }
    }
    if results.is_empty() {
        Ok(Value::Null)
    } else {
        Ok(i.make_array(results))
    }
}
fn re_sym_search(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.search]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let prev = ab(i.get_member(&this, "lastIndex"))?;
    if !same_value(&prev, &Value::Num(0.0)) {
        set_throw(i, &this, "lastIndex", Value::Num(0.0))?;
    }
    let result = regexp_exec_abstract(i, &this, s)?;
    let cur = ab(i.get_member(&this, "lastIndex"))?;
    if !same_value(&cur, &prev) {
        set_throw(i, &this, "lastIndex", prev)?;
    }
    match result {
        Value::Null => Ok(Value::Num(-1.0)),
        _ => ab(i.get_member(&result, "index")),
    }
}
pub(super) fn re_sym_replace(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    re_sym_replace_impl(i, this, a, false)
}

/// Committed dead-result `@@replace` for a direct RegExp whose symbol method, flag accessors, and
/// exec method have all been proven canonical by the caller. The skipped wrapper operations are
/// therefore side-effect-free; matcher state, legacy statics, `lastIndex`, and empty-match
/// advancement remain identical to the builtin algorithm.
pub(super) fn re_sym_replace_discard_direct(
    i: &mut Interp,
    obj: &Gc,
    re: &Rc<crate::regex::Regex>,
    input: &crate::lstr::LStr,
) -> Result<Value, Value> {
    let global = re.global;
    let unicode = re.unicode;
    if global {
        obj.borrow_mut()
            .props
            .get_mut("lastIndex")
            .expect("direct replace lastIndex guard")
            .set_value(Value::Num(0.0));
    }
    loop {
        let matched = regexp_exec_discard_direct(i, obj, re, input)?;
        if !matched || !global {
            break;
        }
        let empty = i
            .regexp_last
            .as_ref()
            .and_then(|last| last.caps.first().copied().flatten())
            .is_some_and(|(start, end)| start == end);
        if empty {
            let li = match obj.borrow().props.get("lastIndex").map(Property::value) {
                Some(Value::Num(n)) => n as usize,
                _ => unreachable!("direct replace lastIndex guard"),
            };
            let next = advance_string_index(li, input, unicode);
            obj.borrow_mut()
                .props
                .get_mut("lastIndex")
                .expect("direct replace lastIndex guard")
                .set_value(Value::Num(next as f64));
        }
    }
    Ok(Value::Undefined)
}

fn re_sym_replace_impl(
    i: &mut Interp,
    this: Value,
    a: &[Value],
    discard_result: bool,
) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.replace]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    // All positions are UTF-16 unit offsets.
    let sunits: Vec<u16> = crate::jstr::units(&s);
    let size = sunits.len();
    let repl = arg(a, 1);
    let functional = repl.is_callable();
    let repl_str = if functional {
        crate::lstr::LStr::from("")
    } else {
        ab(i.to_string(&repl))?
    };
    // Globalness/unicodeness come from the `flags` string (whose getter Gets each flag prop).
    let flags_v = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags_v))?;
    let global = flags.contains('g');
    let unicode = flags.contains('u') || flags.contains('v');
    if global {
        set_throw(i, &this, "lastIndex", Value::Num(0.0))?;
    }
    // When String#replace's result is dead, a direct ordinary RegExp can execute without
    // materializing match arrays or the replacement string. All observable flag Gets and
    // replacement coercion above have already happened. The live exec identity/prototype and
    // internal flags are checked before entering the allocation-free matcher.
    if discard_result && !functional {
        if let Value::Obj(obj) = &this {
            let ptr = Rc::as_ptr(obj) as usize;
            let re = i.regexps.get(&ptr).cloned();
            let direct_exec = re.as_ref().is_some_and(|re| {
                let b = obj.borrow();
                let direct_proto = b
                    .proto
                    .as_ref()
                    .zip(i.extra_protos.get("RegExp"))
                    .is_some_and(|(a, b)| Rc::ptr_eq(a, b));
                if !matches!(b.exotic, Exotic::None)
                    || b.props.contains("exec")
                    || !direct_proto
                    || re.flags != flags.as_ref()
                {
                    return false;
                }
                drop(b);
                i.extra_protos
                    .get("RegExp")
                    .and_then(|proto| {
                        let p = proto.borrow();
                        p.props
                            .get("exec")
                            .filter(|prop| !prop.accessor())
                            .map(Property::value)
                    })
                    .is_some_and(|value| {
                        let Value::Obj(exec) = value else {
                            return false;
                        };
                        let canonical = matches!(
                            exec.borrow().call,
                            Callable::Native(nf)
                                if nf as usize == regexp_exec as *const () as usize
                        );
                        canonical
                    })
            });
            if direct_exec {
                loop {
                    let matched = regexp_exec_discard_fast(i, &this, &s)
                        .expect("direct regexp discard guards")
                        .map_err(|v| v)?;
                    if !matched || !global {
                        break;
                    }
                    let empty = i
                        .regexp_last
                        .as_ref()
                        .and_then(|last| last.caps.first().copied().flatten())
                        .is_some_and(|(start, end)| start == end);
                    if empty {
                        let li = match obj.borrow().props.get("lastIndex").map(Property::value) {
                            Some(Value::Num(n)) => n as usize,
                            _ => unreachable!("discard matcher guarded lastIndex"),
                        };
                        let next = advance_string_index(li, &s, unicode);
                        obj.borrow_mut()
                            .props
                            .get_mut("lastIndex")
                            .expect("discard matcher guarded lastIndex")
                            .set_value(Value::Num(next as f64));
                    }
                }
                return Ok(Value::Undefined);
            }
        }
    }
    // Collect every match (RegExpExec advances lastIndex; empty matches step forward manually).
    let mut results: Vec<Value> = Vec::new();
    loop {
        let result = regexp_exec_abstract(i, &this, s.clone())?;
        if matches!(result, Value::Null) {
            break;
        }
        results.push(result.clone());
        if !global {
            break;
        }
        let m0_v = ab(i.get_member(&result, "0"))?;
        let m0 = ab(i.to_string(&m0_v))?;
        if m0.is_empty() {
            let li_v = ab(i.get_member(&this, "lastIndex"))?;
            let li = to_length_val(i, &li_v)?;
            let next = advance_string_index(li, &s, unicode);
            set_throw(i, &this, "lastIndex", Value::Num(next as f64))?;
        }
    }
    let mut accumulated = String::new();
    let mut next_pos = 0usize;
    for result in &results {
        let length_v = ab(i.get_member(result, "length"))?;
        let length = to_length_val(i, &length_v)?;
        let ncaptures = length.saturating_sub(1);
        let matched_v = ab(i.get_member(result, "0"))?;
        let matched = ab(i.to_string(&matched_v))?;
        let match_len = crate::jstr::unit_len(&matched);
        let index_v = ab(i.get_member(result, "index"))?;
        let pos_raw = ab(i.to_number(&index_v))?;
        let position = if pos_raw.is_nan() || pos_raw < 0.0 {
            0
        } else {
            (pos_raw as usize).min(size)
        };
        let mut captures: Vec<Value> = Vec::new();
        for n in 1..=ncaptures {
            let cap = ab(i.get_member(result, &n.to_string()))?;
            captures.push(if matches!(cap, Value::Undefined) {
                Value::Undefined
            } else {
                Value::Str(ab(i.to_string(&cap))?)
            });
        }
        let mut named = ab(i.get_member(result, "groups"))?;
        // Non-functional replace ToObjects a present `groups` (null → TypeError).
        if !functional && !matches!(named, Value::Undefined) && !matches!(named, Value::Obj(_)) {
            match named {
                Value::Null => {
                    return Err(i.make_error("TypeError", "cannot convert null to object"));
                }
                other => named = box_primitive(i, other),
            }
        }
        let replacement = if functional {
            let mut cbargs = vec![Value::Str(matched.clone())];
            cbargs.extend(captures.iter().cloned());
            cbargs.push(Value::Num(position as f64));
            cbargs.push(Value::Str(s.clone()));
            if !matches!(named, Value::Undefined) {
                cbargs.push(named.clone());
            }
            let r = ab(i.call(repl.clone(), Value::Undefined, &cbargs))?;
            ab(i.to_string(&r))?.to_string()
        } else {
            get_substitution(i, &matched, &sunits, position, &captures, &named, &repl_str)?
        };
        if position >= next_pos {
            accumulated.push_str(&crate::jstr::from_units(&sunits[next_pos..position]));
            accumulated.push_str(&replacement);
            // Pathological expansion ($1 of a huge match, repeated) dies as a RangeError.
            if accumulated.len() > MAX_STR_LEN {
                return Err(i.make_error("RangeError", "Invalid string length"));
            }
            next_pos = position + match_len;
        }
    }
    if next_pos < size {
        accumulated.push_str(&crate::jstr::from_units(&sunits[next_pos..]));
    }
    Ok(Value::from_string(
        crate::jstr::canonicalize(&accumulated).unwrap_or(accumulated),
    ))
}

/// GetSubstitution: expand a `$`-template against a match. `captures` are already `Value::Str` /
/// `Value::Undefined`; `named` is the match's `groups` object (or `undefined`).
fn get_substitution(
    i: &mut Interp,
    matched: &str,
    sunits: &[u16],
    position: usize,
    captures: &[Value],
    named: &Value,
    template: &str,
) -> Result<String, Value> {
    let tail = (position + crate::jstr::unit_len(matched)).min(sunits.len());
    let push_cap = |out: &mut String, cap: &Value| {
        if let Value::Str(s) = cap {
            out.push_str(s);
        }
    };
    let t: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut k = 0;
    while k < t.len() {
        // A pathological template ($1 of a megabyte match, tens of thousands of times) must die
        // as a RangeError before it exhausts memory.
        if out.len() > MAX_STR_LEN {
            return Err(i.make_error("RangeError", "Invalid string length"));
        }
        if t[k] != '$' || k + 1 >= t.len() {
            out.push(t[k]);
            k += 1;
            continue;
        }
        match t[k + 1] {
            '$' => {
                out.push('$');
                k += 2;
            }
            '&' => {
                out.push_str(matched);
                k += 2;
            }
            '`' => {
                out.push_str(&crate::jstr::from_units(
                    &sunits[..position.min(sunits.len())],
                ));
                k += 2;
            }
            '\'' => {
                out.push_str(&crate::jstr::from_units(&sunits[tail..]));
                k += 2;
            }
            '<' if !matches!(named, Value::Undefined) => {
                if let Some(rel) = t[k + 2..].iter().position(|&c| c == '>') {
                    let name: String = t[k + 2..k + 2 + rel].iter().collect();
                    let v = ab(i.get_member(named, &name))?;
                    if !matches!(v, Value::Undefined) {
                        out.push_str(&ab(i.to_string(&v))?);
                    }
                    k = k + 2 + rel + 1;
                } else {
                    out.push('$');
                    k += 1;
                }
            }
            d if d.is_ascii_digit() => {
                let one = d.to_digit(10).unwrap() as usize;
                let two = if k + 2 < t.len() && t[k + 2].is_ascii_digit() {
                    Some(one * 10 + t[k + 2].to_digit(10).unwrap() as usize)
                } else {
                    None
                };
                // Prefer a two-digit group reference when it is in range.
                if let Some(tw) = two {
                    if tw >= 1 && tw <= captures.len() {
                        push_cap(&mut out, &captures[tw - 1]);
                        k += 3;
                        continue;
                    }
                }
                if one >= 1 && one <= captures.len() {
                    push_cap(&mut out, &captures[one - 1]);
                    k += 2;
                } else {
                    out.push('$');
                    k += 1;
                }
            }
            _ => {
                out.push('$');
                k += 1;
            }
        }
    }
    Ok(out)
}
fn re_sym_matchall(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.matchAll]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "RegExp"))?;
    let c = species_constructor(i, &this, &default_ctor)?;
    let flags_v = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags_v))?;
    let matcher = ab(i.construct(c, &[this.clone(), Value::Str(flags.clone())]))?;
    let li_v = ab(i.get_member(&this, "lastIndex"))?;
    let last = to_length_val(i, &li_v)?;
    ab(i.set_member(&matcher, "lastIndex", Value::Num(last as f64)))?;
    let global = flags.contains('g');
    let unicode = flags.contains('u') || flags.contains('v');
    Ok(create_regexp_string_iterator(
        i,
        matcher,
        s.into(),
        global,
        unicode,
    ))
}
/// Execute a dead-result split for the fully canonical direct-RegExp case.
///
/// Every potentially observable lookup performed by String#split/RegExp@@split is first proven
/// to resolve to the realm intrinsics. The matcher still runs and updates the legacy RegExp
/// statics; only the unobservable sticky clone, match arrays, substrings, and final result array
/// are omitted. Returning `None` means no state was touched and the caller must use the builtin.
pub(super) fn re_sym_split_discard_fast(
    i: &mut Interp,
    input: &Value,
    separator: &Value,
    limit: &Value,
) -> Option<Result<Value, Value>> {
    let (Value::Str(input), Value::Obj(obj), Value::Undefined) = (input, separator, limit) else {
        return None;
    };
    let ptr = Rc::as_ptr(obj) as usize;
    let re = i.regexps.get(&ptr)?.clone();
    let proto = i.extra_protos.get("RegExp")?.clone();
    let ctor = i.extra_protos.get("%RegExpCtor%")?.clone();
    let split_key = well_known_key(i, "split")?;
    let species_key = well_known_key(i, "species")?;

    {
        let b = obj.borrow();
        if !matches!(b.exotic, Exotic::None)
            || b.proto.as_ref().is_none_or(|p| !Rc::ptr_eq(p, &proto))
            || b.props.contains(&split_key)
            || [
                "constructor",
                "flags",
                "hasIndices",
                "global",
                "ignoreCase",
                "multiline",
                "dotAll",
                "unicode",
                "unicodeSets",
                "sticky",
            ]
            .iter()
            .any(|name| b.props.contains(name))
        {
            return None;
        }
    }
    if !literal_match_dependencies_canonical(i) {
        return None;
    }
    {
        let p = proto.borrow();
        let split_ok = p
            .props
            .get(&split_key)
            .filter(|property| !property.accessor())
            .map(Property::value)
            .is_some_and(|value| {
                matches!(
                    value,
                    Value::Obj(ref function)
                        if matches!(
                            function.borrow().call,
                            Callable::Native(native)
                                if native as usize == re_sym_split as *const () as usize
                        )
                )
            });
        let constructor_ok = p
            .props
            .get("constructor")
            .filter(|property| !property.accessor())
            .map(Property::value)
            .is_some_and(
                |value| matches!(value, Value::Obj(ref found) if Rc::ptr_eq(found, &ctor)),
            );
        if !split_ok || !constructor_ok {
            return None;
        }
    }
    {
        let c = ctor.borrow();
        let species_ok = c.props.get(&species_key).is_some_and(|property| {
            matches!(
                property.getter(),
                Some(Value::Obj(function))
                    if matches!(
                        function.borrow().call,
                        Callable::Native(native)
                            if native as usize == super::nf_species_getter as *const () as usize
                    )
            ) && property.setter().is_none()
        });
        let prototype_ok = c
            .props
            .get("prototype")
            .filter(|property| !property.accessor())
            .map(Property::value)
            .is_some_and(
                |value| matches!(value, Value::Obj(ref found) if Rc::ptr_eq(found, &proto)),
            );
        if !species_ok || !prototype_ok {
            return None;
        }
    }
    // The default constructor Get is observable, but the exact value is irrelevant here because
    // the guarded intrinsic @@species resolves to `ctor`. A plain own data property therefore
    // proves the Get cannot run user code even if application code replaced globalThis.RegExp.
    let global_ctor_ok = i
        .global
        .borrow()
        .props
        .get("RegExp")
        .is_some_and(|property| !property.accessor());
    if !global_ctor_ok {
        return None;
    }

    // A sticky split loop that advances one unit after every miss is equivalent to one ordinary
    // forward search for its next success. Removing `y` performs that search in the matcher while
    // preserving the successful captures and their legacy-static side effects.
    let search_re = if re.sticky {
        let flags: String = re.flags.chars().filter(|&flag| flag != 'y').collect();
        match i.compiled_regexp(&re.source, &flags) {
            Ok(search) => search,
            Err(Abrupt::Throw(value)) => return Some(Err(value)),
            Err(_) => unreachable!("regexp compilation only throws"),
        }
    } else {
        re
    };
    let text = i.re_text(search_re.unicode, input);
    let size = text.unit_index(text.len());
    if size == 0 {
        if let Some(whole) = search_re.find_text_shared(&text, 0) {
            update_regexp_legacy_statics_lazy(i, &search_re, whole, 0, &text, input);
        }
        return Some(Ok(Value::Undefined));
    }

    let unicode = search_re.unicode;
    let mut p = 0usize;
    let mut q = 0usize;
    while q < size {
        let search_start = text.elem_at_unit(q);
        let Some(whole @ (a, e)) = search_re.find_text_shared(&text, search_start) else {
            break;
        };
        let a = text.unit_index(a);
        let e = text.unit_index(e);
        // RegExp@@split's sticky loop stops before attempting a match at the final position.
        if a >= size {
            break;
        }
        update_regexp_legacy_statics_lazy(i, &search_re, whole, search_start, &text, input);
        if e == p {
            q = advance_string_index(a, input, unicode);
        } else {
            p = e;
            q = p;
        }
    }
    Some(Ok(Value::Undefined))
}

pub(super) fn re_sym_split(i: &mut Interp, this: Value, a: &[Value]) -> Result<Value, Value> {
    require_regexp_this(i, &this, "[Symbol.split]")?;
    let s = ab(i.to_string(&arg(a, 0)))?;
    // All positions are UTF-16 unit offsets.
    let sunits: Vec<u16> = crate::jstr::units(&s);
    let size = sunits.len();
    // SpeciesConstructor(rx, %RegExp%), then a sticky-flagged splitter constructed from it.
    let default_ctor = ab(i.get_member(&Value::Obj(i.global.clone()), "RegExp"))?;
    let c = species_constructor(i, &this, &default_ctor)?;
    let flags_v = ab(i.get_member(&this, "flags"))?;
    let flags = ab(i.to_string(&flags_v))?;
    let unicode = flags.contains('u') || flags.contains('v');
    let new_flags = if flags.contains('y') {
        flags.to_string()
    } else {
        format!("{flags}y")
    };
    let splitter = ab(i.construct(c, &[this.clone(), Value::from_string(new_flags)]))?;
    let limit = match arg(a, 1) {
        Value::Undefined => u32::MAX as usize,
        v => {
            // ToUint32: modular, so a negative limit is a huge one.
            let n = ab(i.to_number(&v))?;
            if n.is_nan() || n.is_infinite() || n == 0.0 {
                0
            } else {
                (n.trunc() as i64).rem_euclid(1i64 << 32) as usize
            }
        }
    };
    let mut out: Vec<Value> = Vec::new();
    if limit == 0 {
        return Ok(i.make_array(out));
    }
    if size == 0 {
        let z = regexp_exec_abstract(i, &splitter, s.clone())?;
        if !matches!(z, Value::Null) {
            return Ok(i.make_array(out));
        }
        out.push(Value::Str(s));
        return Ok(i.make_array(out));
    }
    let mut p = 0usize;
    let mut q = 0usize;
    while q < size {
        ab(i.set_member(&splitter, "lastIndex", Value::Num(q as f64)))?;
        let z = regexp_exec_abstract(i, &splitter, s.clone())?;
        if matches!(z, Value::Null) {
            q = advance_string_index(q, &s, unicode);
            continue;
        }
        let li_v = ab(i.get_member(&splitter, "lastIndex"))?;
        let e = to_length_val(i, &li_v)?.min(size);
        if e == p {
            q = advance_string_index(q, &s, unicode);
            continue;
        }
        out.push(Value::from_string(crate::jstr::from_units(&sunits[p..q])));
        if out.len() == limit {
            return Ok(i.make_array(out));
        }
        p = e;
        let len_v = ab(i.get_member(&z, "length"))?;
        let ncaptures = to_length_val(i, &len_v)?.saturating_sub(1);
        for n in 1..=ncaptures {
            let cap = ab(i.get_member(&z, &n.to_string()))?;
            out.push(cap);
            if out.len() == limit {
                return Ok(i.make_array(out));
            }
        }
        q = p;
    }
    out.push(Value::from_string(crate::jstr::from_units(
        &sunits[p..size],
    )));
    Ok(i.make_array(out))
}
