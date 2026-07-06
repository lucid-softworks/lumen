// A small Hono application, built on Web Standard Request/Response.
//
// Hono's core is just a router that turns a `Request` into a `Response` via
// `app.fetch(request)`. It needs no listening socket to run, which makes it a
// good fit for the lumen runtime today: lumen ships the WinterTC web platform
// (Request/Response/Headers/URL) but not yet an HTTP *server*, so we drive the
// app by dispatching requests through `app.fetch` directly (see serve.js).

import { Hono } from 'hono';

const app = new Hono();

// A plain text route.
app.get('/', (c) => c.text('Hello from Hono on lumen!'));

// JSON response with a header set by middleware.
app.use('/api/*', async (c, next) => {
  await next();
  c.header('X-Powered-By', 'lumen');
});

app.get('/api/hello', (c) => {
  const name = c.req.query('name') ?? 'world';
  return c.json({ message: `hello, ${name}`, runtime: 'lumen' });
});

// Path params.
app.get('/users/:id', (c) => {
  return c.json({ id: c.req.param('id') });
});

// POST with a JSON body echoed back.
app.post('/echo', async (c) => {
  const body = await c.req.json();
  return c.json({ youSent: body }, 201);
});

// 404 fallback is built in; add a custom one to show it works.
app.notFound((c) => c.text('nothing here\n', 404));

export default app;
