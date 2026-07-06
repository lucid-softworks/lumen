// ky is a tiny HTTP client built on fetch. lumen ships fetch (http only), so ky runs unmodified.
// To keep the demo self-contained we spin up a local node:http server and call it with ky.
import { createServer } from 'node:http';
import ky from 'ky';

const server = createServer((req, res) => {
  let body = '';
  req.on('data', (c) => (body += c));
  req.on('end', () => {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ method: req.method, url: req.url, echo: body || null }));
  });
});

await new Promise((r) => server.listen(0, r));
const base = `http://127.0.0.1:${server.address().port}`;

// GET with ky, parsed as JSON.
const got = await ky.get(`${base}/hello?name=lumen`).json();
console.log('GET  ->', JSON.stringify(got));

// POST a JSON body with ky.
const posted = await ky.post(`${base}/submit`, { json: { hi: 'there' } }).json();
console.log('POST ->', JSON.stringify(posted));

// ky's retry/hooks: add a header via a beforeRequest hook.
const withHook = await ky.get(`${base}/hooked`, {
  hooks: { beforeRequest: [(request) => request.headers.set('x-demo', 'ky-on-lumen')] },
}).json();
console.log('HOOK ->', JSON.stringify(withHook));

server.close();
