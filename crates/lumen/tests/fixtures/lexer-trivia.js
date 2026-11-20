function assert(value, message) { if (!value) throw new Error(message); }
const lines = ['\n', '\r', '\r\n', '\u2028', '\u2029'];
for (const line of lines) {
    for (const separator of [line, '/* € 💡' + line + '** α */', '// € 💡 " */' + line]) {
        assert(eval('(function(){return' + separator + '42})()') === undefined, 'return ASI');
        let caught = false;
        try { eval('(function(){throw' + separator + '42})()'); }
        catch (error) { caught = error instanceof SyntaxError; }
        assert(caught, 'throw line terminator');
        assert(eval('var triviaX = 1; triviaX' + separator + '++triviaX') === 2, 'postfix ASI');
    }
    assert(eval('<!-- € 💡' + line + '42') === 42, 'HTML opening comment');
    assert(eval('--> € 💡' + line + '42') === 42, 'HTML closing comment');
    assert(eval('#! /bin/💡' + line + '42') === 42, 'hashbang terminator');
}
for (const separator of [' \t\v\f ', '\u00a0\ufeff', '/**/', '/* € 💡 " // * ** α */']) {
    assert(eval('(function(){return' + separator + '42})()') === 42, 'non-newline trivia');
}
for (const source of ['/*', '/* € 💡', '/*\n α *', '/*\u2028']) {
    let caught = false;
    try { eval(source); } catch (error) { caught = error instanceof SyntaxError; }
    assert(caught, 'unterminated comment');
}
const text = 'function λ(α) { /* 💡\u2028 € */\n  return α + 1; // β\n}';
const f = eval('(' + text + ')');
assert(f(3) === 4 && f.toString() === text, 'source offsets');
assert(eval('/* / " */ /a/.test("a")') === true, 'regex after comment');
'passed';
