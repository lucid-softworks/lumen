function assert(ok) { if (!ok) throw new Error('named function assertion'); }
const compiled_increment = function increment(value) { return value + 1; };
const compiled_defaults = function defaults(value = 7) { return value * 2; };
const compiled_capture = function capture(value) { return () => value; };
const compiled_rest = function gather(head, ...tail) { return [head, tail]; };
const compiled_constructor = function Box(value) { this.value = value; };
const compiled_this = function receiver(value) { return this.base + value; };
const compiled_property = function property(value) { return {property: value}.property; };
const observed_arguments = function calleeRead() { return arguments.callee; };
const observed_strict_arguments = function strictCallee() { 'use strict'; return arguments.callee; };
const fallback_recursive = function recurse(n) { return n ? n * recurse(n - 1) : 1; };
const fallback_self = function self() { return self; };
const fallback_write = function unchanged() { unchanged = 1; return unchanged; };
const fallback_strict_write = function immutable() { 'use strict'; immutable = 1; };
const fallback_capture = function capturedSelf() { return () => capturedSelf; };
const fallback_default = function defaultSelf(value = defaultSelf) { return value; };
const fallback_shadow = function shadow(shadow) { return shadow; };
const fallback_eval = function evaluatedSelf() { return eval('evaluatedSelf'); };
const fallback_template = function templateSelf() { return `${typeof templateSelf}`; };
const marker = {};
function drive() {
    for(let count = 0; count < 200; count++) {
        assert(compiled_increment(count) === count + 1);
        assert(compiled_defaults() === 14 && compiled_defaults(4) === 8);
        assert(compiled_capture(marker)() === marker);
        const rest = compiled_rest(1, marker, 3);
        assert(rest[0] === 1 && rest[1][0] === marker && rest[1][1] === 3);
        assert(new compiled_constructor(marker).value === marker);
        assert(compiled_this.call({base: 10}, 2) === 12);
        assert(compiled_property(marker) === marker);
        assert(observed_arguments(marker, 3) === observed_arguments);
        let strictArgumentsThrew = false;
        try { observed_strict_arguments(); } catch(error) { strictArgumentsThrew = error instanceof TypeError; }
        assert(strictArgumentsThrew);
        assert(fallback_recursive(5) === 120 && fallback_self() === fallback_self);
        assert(fallback_write() === fallback_write && fallback_capture()() === fallback_capture);
        assert(fallback_default() === fallback_default && fallback_default(marker) === marker);
        assert(fallback_shadow(marker) === marker && fallback_eval() === fallback_eval);
        assert(fallback_template() === 'function');
        let threw = false;
        try { fallback_strict_write(); } catch(error) { threw = error instanceof TypeError; }
        assert(threw);
    }
}
drive();
assert(compiled_increment.name === 'increment');
assert(compiled_constructor.name === 'Box');
assert(compiled_increment.toString().includes('function increment(value)'));
function mutableDeclaration() { return mutableDeclaration; }
const originalDeclaration = mutableDeclaration;
mutableDeclaration = marker;
for(let count=0;count<100;count++) assert(originalDeclaration() === marker);
'passed';
