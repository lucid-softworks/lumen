// Parse Djot markup to an AST and render it to HTML, entirely in-process (pure JS parser).
// @djot/djot ships as CommonJS, so we default-import and destructure (lumen exposes a CJS
// module's `module.exports` as the ESM default).
import djot from '@djot/djot';
const { parse, renderHTML } = djot;

const doc = `# Djot on lumen

A *fast* markup language, parsed by a **zero-native** JS engine.

- one
- two
- three

> A blockquote with \`inline code\`.
`;

// Reference output from @djot/djot on Node; check every iteration, not just its length.
const expected = "<section id=\"Djot-on-lumen\">\n<h1>Djot on lumen</h1>\n<p>A <strong>fast</strong> markup language, parsed by a <strong><strong>zero-native</strong></strong> JS engine.</p>\n<ul>\n<li>\none\n</li>\n<li>\ntwo\n</li>\n<li>\nthree\n</li>\n</ul>\n<blockquote>\n<p>A blockquote with <code>inline code</code>.</p>\n</blockquote>\n</section>\n";
const start = Date.now();
let bytes = 0;
for (let i = 0; i < 10000; i++) {
  const html = renderHTML(parse(doc));
  if (html !== expected) throw new Error('Djot output mismatch');
  bytes += html.length;
}
console.log('Djot:', Date.now() - start, 'ms;', bytes, 'HTML characters');
