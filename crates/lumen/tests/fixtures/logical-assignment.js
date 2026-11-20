function check(ok, message) { if (!ok) throw new Error(message); }
function compiled_or(value, rhs) { return value ||= rhs(); }
function compiled_and(value, rhs) { return value &&= rhs(); }
function compiled_nullish(value, rhs) { return value ??= rhs(); }
function compiled_index_or(base, key, rhs) { return base()[key()] ||= rhs(); }
function compiled_index_and(base, key, rhs) { return base()[key()] &&= rhs(); }
function compiled_index_nullish(base, key, rhs) { return base()[key()] ??= rhs(); }
function compiled_member(base, rhs) { return base().value ||= rhs(); }
function compiled_strict(base, rhs) { 'use strict'; return base.value ||= rhs(); }
function compiled_discard(base, rhs) {
    base.value ??= rhs();
    return base.value ||= (base.other ??= rhs());
}
function compiled_captured(value, rhs) {
    const read = () => value;
    value ||= rhs();
    return read();
}
var logicalGlobal;
function compiled_global(rhs) { return logicalGlobal ??= rhs(); }
function compiled_inferred() { let inferred; inferred ||= function(){}; return inferred.name; }
function fallback_const(rhs) { const value = 1; return value ||= rhs(); }
function fallback_const_write(rhs) { const value = 0; return value ||= rhs(); }

for (let round = 0; round < 24; round++) {
    for (const value of [undefined, null, false, 0, -0, NaN, '', 1, 'x', {}, 0n, 1n]) {
        for (const [local, indexed, shouldWrite] of [
            [compiled_or, compiled_index_or, !value],
            [compiled_and, compiled_index_and, !!value],
            [compiled_nullish, compiled_index_nullish, value === null || value === undefined]
        ]) {
            let calls = 0;
            const replacement = {};
            const rhs = () => { calls++; return replacement; };
            check(Object.is(local(value, rhs), shouldWrite ? replacement : value), 'local result');
            check(calls === (shouldWrite ? 1 : 0), 'local short circuit');
            const events = [];
            let stored = value;
            const object = {
                get field() { events.push('get'); return stored; },
                set field(v) { events.push('set'); stored = v; }
            };
            const key = { [Symbol.toPrimitive]() { events.push('coerce'); return 'field'; } };
            const result = indexed(
                () => { events.push('base'); return object; },
                () => { events.push('key'); return key; },
                () => { events.push('rhs'); return replacement; }
            );
            check(Object.is(result, shouldWrite ? replacement : value), 'property result');
            check(Object.is(stored, shouldWrite ? replacement : value), 'property storage');
            check(events.join(',') === (shouldWrite ? 'base,key,coerce,get,rhs,set' : 'base,key,coerce,get'), 'property order');
        }
    }
    const first = {field: 0}, second = {field: 100};
    let current = first;
    check(compiled_index_or(() => current, () => 'field', () => { current = second; return 7; }) === 7, 'result after base change');
    check(first.field === 7 && second.field === 100, 'original reference retained');
    const symbol = Symbol('field'), object = {[symbol]: undefined};
    check(compiled_index_nullish(() => object, () => symbol, () => 8) === 8 && object[symbol] === 8, 'symbol key');
    let calls = 0;
    check(compiled_member(() => ({value: 2}), () => { calls++; return 9; }) === 2 && calls === 0, 'named member short circuit');
    check(compiled_captured(0, () => 9) === 9 && compiled_captured(3, () => 9) === 3, 'captured binding');
    logicalGlobal = undefined;
    check(compiled_global(() => 10) === 10 && logicalGlobal === 10, 'free binding write');
    check(compiled_global(() => { throw new Error('skipped'); }) === 10, 'free binding skip');
    check(compiled_discard({value: null}, () => 11) === 11, 'discarded expression');
    const nestedSkip = {value: 0, other: 17};
    check(compiled_discard(nestedSkip, () => { throw new Error('skipped'); }) === 17 && nestedSkip.value === 17, 'nested skipped reference');
    const nestedWrite = {value: 0, other: null};
    check(compiled_discard(nestedWrite, () => 18) === 18 && nestedWrite.value === 18 && nestedWrite.other === 18, 'nested assignment references');
    check(compiled_inferred() === 'inferred', 'inferred function name');
    check(fallback_const(() => { throw new Error('skipped'); }) === 1, 'constant skip');
    let threw = false;
    try { fallback_const_write(() => 2); } catch (error) { threw = error instanceof TypeError; }
    check(threw, 'constant write');
    const frozen = Object.freeze({value: 1});
    check(compiled_strict(frozen, () => { throw new Error('skipped'); }) === 1, 'strict skipped write');
    calls = 0; threw = false;
    try { compiled_strict(Object.freeze({value: 0}), () => { calls++; return 12; }); }
    catch (error) { threw = error instanceof TypeError; }
    check(threw && calls === 1, 'strict write order');
    let sets = 0;
    const proxy = new Proxy({value: 1}, {set() { sets++; return false; }});
    check(compiled_strict(proxy, () => 2) === 1 && sets === 0, 'proxy skipped setter');
    const sentinel = {};
    threw = false;
    try { compiled_member(() => ({value: 0}), () => { throw sentinel; }); }
    catch (error) { threw = error === sentinel; }
    check(threw, 'RHS throw');
}
'passed';
