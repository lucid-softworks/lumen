function assert(ok, message) { if (!ok) throw new Error(message); }
function typeError(fn, message) {
    let caught = false;
    try { fn(); } catch (error) { caught = error instanceof TypeError; }
    assert(caught, message);
}
function into(values, target) {
    values.constructor = { [Symbol.species]: function() { return target; } };
    return values;
}
function checkData(object, key, value) {
    const descriptor = Object.getOwnPropertyDescriptor(object, key);
    assert(descriptor.value === value && descriptor.writable && descriptor.enumerable &&
           descriptor.configurable && !('get' in descriptor), 'data descriptor ' + key);
}
const token = {};
const sparse = [token, , 3].map(value => value);
assert(sparse.length === 3 && !(1 in sparse), 'map preserves holes');
checkData(sparse, '0', token);
checkData(sparse, '2', 3);
assert([1, , 3, 4].filter(value => value > 2).join() === '3,4', 'filter');
assert([1, [2, , 3]].flat().join() === '1,2,3', 'flat');
const spliced = [1, , 3, 4].splice(0, 3);
assert(spliced.length === 3 && !(1 in spliced) && spliced[2] === 3, 'splice holes');

const inherited = Object.create({set 0(value) { throw new Error('inherited setter'); }});
assert(into([7], inherited).map(value => value) === inherited, 'species identity');
checkData(inherited, '0', 7);
const accessor = {};
Object.defineProperty(accessor, '0', {
    configurable: true,
    get() { throw new Error('own getter'); },
    set(value) { throw new Error('own setter'); }
});
into([8], accessor).map(value => value);
checkData(accessor, '0', 8);
const fixed = {};
Object.defineProperty(fixed, '0', {value: 9});
typeError(() => into([9], fixed).map(value => value), 'nonconfigurable destination');
typeError(() => into([1], Object.preventExtensions({})).map(value => value), 'nonextensible');
const partial = {};
typeError(() => into([1, 2], partial).map((value, index) => {
    if (index === 1) Object.preventExtensions(partial);
    return value;
}), 'callback prevents extension');
assert(partial[0] === 1 && !('1' in partial), 'previous writes retained');
const fixedLength = [];
Object.defineProperty(fixedLength, 'length', {writable: false});
typeError(() => into([1], fixedLength).map(value => value), 'array length restriction');
const bytes = new Uint8Array(2);
into([300, -1], bytes).map(value => value);
assert(bytes[0] === 44 && bytes[1] === 255, 'typed array coercion');
function mapped(value) { return {args: arguments, read() { return value; }}; }
const alias = mapped(1);
into([9], alias.args).map(value => value);
assert(alias.read() === 9 && alias.args[0] === 9, 'mapped argument update');

const seen = [];
const destination = {};
const proxy = new Proxy(destination, {defineProperty(target, key, descriptor) {
    assert(Object.getPrototypeOf(descriptor) === Object.prototype, 'trap descriptor prototype');
    assert(Object.keys(descriptor).join() === 'value,writable,enumerable,configurable', 'trap field order');
    assert(descriptor.writable && descriptor.enumerable && descriptor.configurable, 'trap attributes');
    seen.push(descriptor);
    return Reflect.defineProperty(target, key, descriptor);
}});
into([5, 6], proxy).map(value => value);
assert(seen.length === 2 && seen[0] !== seen[1] && destination[1] === 6, 'fresh trap descriptors');
typeError(() => into([1], new Proxy({}, {defineProperty() { return false; }})).map(x => x), 'false trap');
const revoked = Proxy.revocable({}, {});
revoked.revoke();
typeError(() => into([1], revoked.proxy).map(x => x), 'revoked proxy');
const nestedTarget = {};
into([4], new Proxy(new Proxy(nestedTarget, {}), {})).map(x => x);
checkData(nestedTarget, '0', 4);

let reviverTraps = 0;
const reviverTarget = {x: 2};
const parsed = JSON.parse('{"a":1,"b":{"x":2}}', function(key, value) {
    if (key === 'a') this.b = new Proxy(reviverTarget, {defineProperty() { reviverTraps++; return false; }});
    return key === 'x' ? 99 : value;
});
assert(reviverTraps === 1 && parsed.b.x === 2, 'reviver ignores false trap');
const removed = JSON.parse('{"x":1}', function(key, value) {
    if (key === 'x') { delete this.x; Object.preventExtensions(this); }
    return value;
});
assert(!('x' in removed), 'reviver ignores failed creation');

const publicReads = [];
Object.defineProperty({}, 'x', new Proxy({value: 3, writable: true, enumerable: true, configurable: true}, {
    has(target, key) { publicReads.push('has:' + key); return key in target; },
    get(target, key) { publicReads.push('get:' + key); return target[key]; }
}));
assert(publicReads.join() === 'has:enumerable,get:enumerable,has:configurable,get:configurable,has:value,get:value,has:writable,get:writable,has:get,has:set', 'public descriptor access order');

let poisonedReads = 0;
const poisonDescriptor = Object.create(null);
poisonDescriptor.configurable = true;
poisonDescriptor.get = function() { poisonedReads++; throw new Error('inherited descriptor field'); };
Object.defineProperty(Object.prototype, 'get', poisonDescriptor);
Object.defineProperty(Object.prototype, 'set', poisonDescriptor);
try {
    assert([1, 2].map(x => x + 1).join() === '2,3', 'polluted map');
    assert([1, 2].filter(x => x > 1).join() === '2', 'polluted filter');
    assert([[1], [2]].flat().join() === '1,2', 'polluted flat');
    assert([1, 2].splice(0, 1).join() === '1', 'polluted splice');
    assert(JSON.parse('{"x":1}', (key, value) => key === 'x' ? 2 : value).x === 2, 'polluted reviver');
    const observed = {};
    into([7], new Proxy(observed, {defineProperty(target, key, descriptor) {
        assert(Object.getPrototypeOf(descriptor) === Object.prototype, 'polluted trap prototype');
        assert(Object.keys(descriptor).join() === 'value,writable,enumerable,configurable', 'polluted trap keys');
        target[key] = descriptor.value;
        return true;
    }})).map(x => x);
    assert(observed[0] === 7 && poisonedReads === 0, 'internal records ignore prototype pollution');
} finally {
    delete Object.prototype.get;
    delete Object.prototype.set;
}
'passed';
