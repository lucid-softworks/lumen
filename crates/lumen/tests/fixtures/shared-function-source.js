function sourceAssert(value, message) { if (!value) throw new Error(message); }
function outer(π) { /* 🌙 */ function inner() { return π; } return inner; }
class Café { constructor() {} method() { return '🌙'; } }
const savedInner = outer(7);
const templateSource = `${function templateInner(){return 'é';}}`;
function sourceTag(parts, value) { return value; }
const taggedSource = sourceTag`💡${function taggedInner(){return 'λ';}}`;
const savedArrow = eval('/* é */ (π) => /* 🌙 */ π + 1');
const savedCrlf = eval('(function crlf() {\r\n return "é";\r\n})');
const sourceGetter = { get item() { return 3; } };
const savedGetter = Object.getOwnPropertyDescriptor(sourceGetter, 'item').get;
function verifySavedSources() {
    sourceAssert(outer.toString() === 'function outer(π) { /* 🌙 */ function inner() { return π; } return inner; }', 'outer source');
    sourceAssert(savedInner.toString() === 'function inner() { return π; }' && savedInner() === 7, 'nested source');
    sourceAssert(Café.toString() === "class Café { constructor() {} method() { return '🌙'; } }", 'class source');
    sourceAssert(Café.prototype.method.toString() === "method() { return '🌙'; }", 'method source');
    sourceAssert(templateSource === "function templateInner(){return 'é';}", 'template source');
    sourceAssert(taggedSource.toString() === "function taggedInner(){return 'λ';}", 'tagged source');
    sourceAssert(savedArrow.toString() === '(π) => /* 🌙 */ π + 1' && savedArrow(4) === 5, 'arrow source');
    sourceAssert(savedCrlf.toString() === 'function crlf() {\r\n return "é";\r\n}', 'CRLF source');
    sourceAssert(savedGetter.toString() === 'get item() { return 3; }', 'getter source');
    return 'passed';
}
verifySavedSources();
