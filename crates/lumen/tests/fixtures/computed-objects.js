function check(ok, message) { if (!ok) throw new Error(message); }
function compiled_dynamic(key, value) { return {[key]: value}; }
function compiled_mixed(key1, key2, value1, value2) {
    return {start: 1, [key1()]: value1(), [key2()]: value2(), end: 4};
}
function compiled_duplicate(key) { return {[key]: 1, middle: 2, [key]: 3, ['__proto__']: 4}; }
function compiled_functions(key, symbol) {
    return {[key]: function(){}, [symbol]: () => 2, fixed: (() => 3), explicit: function named(){}};
}
function compiled_existing(key, existing) { return {[key]: existing}; }
function compiled_loop(key, value) {
    let result;
    for (let n = 0; n < 3; n++) result = {[key]: value + n};
    return result;
}
function compiled_caught(key, value) {
    try { return {[key()]: value()}; } catch (error) { return error; }
}
const EntitiesKey = Symbol('Entities');
class EntityPathTracker {
    constructor() {
        this.entityPaths = Object.create(null, {[EntitiesKey]: {value: new Set()}});
    }
    getEntities(path) {
        let currentPaths = this.entityPaths;
        for (const pathSegment of path) {
            currentPaths = currentPaths[pathSegment] ||= Object.create(null, {
                [EntitiesKey]: {value: new Set()}
            });
        }
        return currentPaths[EntitiesKey];
    }
}
const tracker = new EntityPathTracker();
for (let round = 0; round < 24; round++) {
    for (const key of ['x', '', '0', 2, -0, null, undefined, true, 3n, Symbol('key')]) {
        const object = compiled_dynamic(key, 7);
        check(object[key] === 7, 'computed value');
        const descriptor = Object.getOwnPropertyDescriptor(object, key);
        check(descriptor.value === 7 && descriptor.writable && descriptor.enumerable && descriptor.configurable, 'data attributes');
        check(Object.getPrototypeOf(object) === Object.prototype, 'ordinary prototype');
    }
    const events = [];
    const first = {[Symbol.toPrimitive](hint) { events.push('coerce1:' + hint); return 'first'; }};
    const second = {[Symbol.toPrimitive](hint) { events.push('coerce2:' + hint); return 'second'; }};
    const mixed = compiled_mixed(
        () => { events.push('key1'); return first; }, () => { events.push('key2'); return second; },
        () => { events.push('value1'); return 2; }, () => { events.push('value2'); return 3; }
    );
    check(events.join(',') === 'key1,coerce1:string,value1,key2,coerce2:string,value2', 'key/value order');
    check(Object.keys(mixed).join(',') === 'start,first,second,end', 'property order');
    const duplicate = compiled_duplicate('same');
    check(Object.keys(duplicate).join(',') === 'same,middle,__proto__' && duplicate.same === 3, 'duplicate property ordering');
    check(duplicate.__proto__ === 4 && Object.getPrototypeOf(duplicate) === Object.prototype, 'computed proto is data');
    const symbol = Symbol('described'), anonymous = Symbol();
    const functions = compiled_functions('dynamic', symbol);
    check(functions.dynamic.name === 'dynamic' && functions[symbol].name === '[described]', 'dynamic function names');
    check(functions.fixed.name === 'fixed' && functions.explicit.name === 'named', 'mixed function names');
    check(compiled_functions('x', anonymous)[anonymous].name === '', 'undescribed symbol name');
    const nameless = (0, function(){});
    check(compiled_existing('keep', nameless).keep.name === '', 'existing function must not be renamed');
    let calls = 0;
    const symbolKey = {[Symbol.toPrimitive]() { calls++; return symbol; }};
    check(compiled_dynamic(symbolKey, 9)[symbol] === 9 && calls === 1, 'object-to-symbol key');
    calls = 0;
    const changing = {[Symbol.toPrimitive]() { return ++calls === 1 ? 'one' : 'two'; }};
    const changed = compiled_duplicate(changing);
    check(calls === 2 && changed.one === 1 && changed.two === 3, 'each key occurrence coerced once');
    check(compiled_loop('value', 10).value === 12, 'loop creates fresh literals');
    const sentinel = {};
    calls = 0;
    check(compiled_caught(() => { throw sentinel; }, () => { calls++; }) === sentinel && calls === 0, 'key throw skips value');
    check(compiled_caught(() => ({[Symbol.toPrimitive]() { throw sentinel; }}), () => { calls++; }) === sentinel && calls === 0, 'coercion throw skips value');
    check(compiled_caught(() => 'key', () => { throw sentinel; }) === sentinel, 'value throw');
    let setters = 0;
    const inheritedSymbol = Symbol('setter');
    Object.defineProperty(Object.prototype, 'literalSetter', {set() { setters++; }, configurable: true});
    Object.defineProperty(Object.prototype, inheritedSymbol, {set() { setters++; }, configurable: true});
    Object.defineProperty(Object.prototype, 'literalLocked', {value: 1, writable: false, configurable: true});
    try {
        check(compiled_dynamic('literalSetter', 20).literalSetter === 20, 'own string definition');
        check(compiled_dynamic(inheritedSymbol, 21)[inheritedSymbol] === 21, 'own symbol definition');
        check(compiled_dynamic('literalLocked', 22).literalLocked === 22, 'inherited nonwritable property');
        check(setters === 0, 'prototype setters bypassed');
    } finally {
        delete Object.prototype.literalSetter;
        delete Object.prototype[inheritedSymbol];
        delete Object.prototype.literalLocked;
    }
    const path = ['root', 'leaf'];
    const entities = tracker.getEntities(path);
    entities.add(round);
    check(tracker.getEntities(path) === entities && entities.size === round + 1, 'compiled path tracker');
}
'passed';
