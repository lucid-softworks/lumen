// One Hono app, served through whichever runtime is running it — so cold-start.py can time the
// same program on lumen, Bun, and Node. Reads PORT from the environment. Bind fast, answer '/'.
import { Hono } from 'hono';

const app = new Hono();
app.get('/', (c) => c.text('hello hono!!\n'));

const port = Number(process.env.PORT || 3000);
const fetch = app.fetch;

if (globalThis.Lumen?.serve) {
  // lumen (this repo).
  Lumen.serve(fetch, { hostname: '127.0.0.1', port });
} else if (globalThis.Bun) {
  // Bun's native web-standard server.
  Bun.serve({ hostname: '127.0.0.1', port, fetch });
} else {
  // Node: a minimal node:http -> web Request/Response adapter (avoids a @hono/node-server dep).
  const { createServer } = await import('node:http');
  const readBody = (req) =>
    new Promise((resolve) => {
      const chunks = [];
      req.on('data', (c) => chunks.push(c));
      req.on('end', () => resolve(Buffer.concat(chunks)));
    });
  createServer(async (req, res) => {
    const url = `http://${req.headers.host || 'localhost'}${req.url}`;
    const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
    const request = new Request(url, {
      method: req.method,
      headers: req.headers,
      body: hasBody ? await readBody(req) : undefined,
    });
    const response = await fetch(request);
    res.writeHead(response.status, Object.fromEntries(response.headers));
    res.end(Buffer.from(await response.arrayBuffer()));
  }).listen(port, '127.0.0.1');
}
