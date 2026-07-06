// Server-side render a React component tree to HTML — no build step (React.createElement,
// so no JSX transform needed) — and serve it over node:http.
const http = require('http');
const React = require('react');
const { renderToString } = require('react-dom/server');

const h = React.createElement;

function Item(props) {
  return h('li', null, props.children);
}
function App(props) {
  return h('div', { className: 'app' },
    h('h1', null, `Hello from React ${React.version} on lumen`),
    h('p', null, `Rendered ${props.count} items server-side:`),
    h('ul', null, props.items.map((it, i) => h(Item, { key: i }, it))),
  );
}

const server = http.createServer((req, res) => {
  const items = ['zero-dependency engine', 'node:http on Lumen.serve', 'react-dom/server'];
  const html = renderToString(h(App, { items, count: items.length }));
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(`<!doctype html><html><body>${html}</body></html>\n`);
});

const port = Number(process.env.PORT || 3000);
server.listen(port, () => console.log(`react SSR on lumen: http://localhost:${port}/`));
