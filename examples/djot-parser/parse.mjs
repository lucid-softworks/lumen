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

const html = renderHTML(parse(doc));
console.log(html);
