// In-process Hono benchmark: dispatch requests through `app.fetch()` and read each response body.
// This is the common denominator across runtimes — lumen has no HTTP server, so a socket-based
// test could not include it. Measures Hono's routing + middleware + Request/Response handling on
// each runtime's JS engine (no network).
//
// Usage: <runtime> bench.mjs [N]     e.g.  node bench.mjs 50000
import { Hono } from 'hono';

const app = new Hono();
app.get('/', (c) => c.text('Hello from Hono'));
app.get('/json', (c) => c.json({ ok: true, n: 42 }));
app.get('/users/:id', (c) => c.json({ id: c.req.param('id') }));
app.use('/api/*', async (c, next) => {
  await next();
  c.header('x-powered-by', 'bench');
});
app.get('/api/hello', (c) => c.json({ hello: c.req.query('name') ?? 'world' }));

const make = [
  () => new Request('http://localhost/'),
  () => new Request('http://localhost/json'),
  () => new Request('http://localhost/users/42'),
  () => new Request('http://localhost/api/hello?name=lumen'),
];

async function run(iters) {
  let sink = 0;
  for (let i = 0; i < iters; i++) {
    const res = await app.fetch(make[i & 3]());
    const body = await res.text();
    sink += res.status + body.length; // keep the work observable
  }
  return sink;
}

const N = Number(process.env.BENCH_N || 50000);
const WARMUP = Math.max(1000, (N / 10) | 0);

await run(WARMUP); // warm the engine / JITs

// best of 3 timed passes
let best = Infinity;
for (let pass = 0; pass < 3; pass++) {
  const t0 = performance.now();
  await run(N);
  const ms = performance.now() - t0;
  if (ms < best) best = ms;
}
const rps = Math.round(N / (best / 1000));
console.log(JSON.stringify({ iters: N, ms: +best.toFixed(1), rps }));
