function assert(ok) { if (!ok) throw new Error('rest parameter assertion'); }
function collect(head, ...tail) { return tail; }
function defaults(head = {fresh: true}, ...tail) { return [head, tail]; }
function captured(head, ...tail) { return () => [head, tail]; }
function reassigned(...tail) { const read = () => tail; tail = [7]; return read; }
function target(a, b, c) { return [a, b, c]; }
function forward(...tail) { return target(...tail); }
function Holder(...items) { this.items = items; }
function withArguments(head, ...tail) { return [arguments.length, arguments[1], tail]; }
function tdz(head = tail, ...tail) { return head; }
const marker = {id: 42};
const arrow = (...items) => items;
const receiver = {method(prefix, ...items) { return [this, prefix, items]; }};
function makeArrow() { return (...items) => [this, items]; }
const lexicalArrow = makeArrow.call(marker);
function drive() {
for (let call = 0; call < 200; call++) {
    const empty = collect();
    const first = collect(0, marker, undefined, 3), second = collect(0, marker, undefined, 3);
    assert(Array.isArray(empty) && empty.length === 0 && empty !== collect());
    assert(first !== second && first.length === 3 && first[0] === marker && first[1] === undefined && first[2] === 3);
    assert(Object.getPrototypeOf(first) === Array.prototype && Object.hasOwn(first, '1'));
    const descriptor = Object.getOwnPropertyDescriptor(first, '0');
    assert(descriptor.writable && descriptor.enumerable && descriptor.configurable);
    first[0] = 99;
    assert(second[0] === marker && marker.id === 42);
    const pair = defaults(undefined, marker, 2);
    assert(pair[0].fresh && pair[1][0] === marker && pair[1][1] === 2);
    assert(defaults()[0] !== defaults()[0] && defaults()[1].length === 0);
    const closure = captured('prefix', marker, 2);
    assert(closure()[0] === 'prefix' && closure()[1] === closure()[1]);
    closure()[1][1] = 9;
    assert(closure()[1][1] === 9 && captured('prefix', marker, 2)()[1][1] === 2);
    assert(reassigned(marker)()[0] === 7);
    const f = forward(marker, 2, 3);
    assert(f[0] === marker && f[1] === 2 && f[2] === 3);
    const holder = new Holder(marker, 2);
    assert(holder.items[0] === marker && holder.items.length === 2 && new Holder().items.length === 0);
    assert(arrow(marker, 2)[0] === marker && arrow().length === 0);
    const method = receiver.method('prefix', marker, 2);
    assert(method[0] === receiver && method[1] === 'prefix' && method[2][0] === marker);
    const lexical = lexicalArrow.call(receiver, 7, 8);
    assert(lexical[0] === marker && lexical[1][0] === 7 && lexical[1][1] === 8);
    const withArgs = withArguments('prefix', marker, 2);
    assert(withArgs[0] === 3 && withArgs[1] === marker && withArgs[2][0] === marker);
    let threw = false;
    try { tdz(); } catch (error) { threw = error instanceof ReferenceError; }
    assert(threw);
}
}
drive();
const originalArray = Array, iterator = Array.prototype[Symbol.iterator];
Array.prototype[Symbol.iterator] = function() { throw new Error('unexpected iterator'); };
Array = function() { throw new Error('public Array constructor called'); };
try {
    for (let call = 0; call < 100; call++) {
        const values = collect(0, marker, 2);
        assert(values.length === 2 && values[0] === marker && Object.getPrototypeOf(values) === originalArray.prototype);
    }
} finally {
    Array = originalArray;
    Array.prototype[Symbol.iterator] = iterator;
}
'passed';
