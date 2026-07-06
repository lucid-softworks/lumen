// Preact SSR via preact-render-to-string, served over node:http. ESM + preact's `h`.
import { createServer } from 'node:http';
import { h } from 'preact';
import { render } from 'preact-render-to-string';

function App({ items }) {
  return h('main', null,
    h('h1', null, 'Hello from Preact on lumen'),
    h('ul', null, items.map((it) => h('li', null, it))),
  );
}

const server = createServer((req, res) => {
  const html = render(h(App, { items: ['fast', 'tiny', 'server-rendered'] }));
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(`<!doctype html><html><body>${html}</body></html>\n`);
});

const port = Number(process.env.PORT || 3000);
server.listen(port, () => console.log(`preact SSR on lumen: http://localhost:${port}/`));
