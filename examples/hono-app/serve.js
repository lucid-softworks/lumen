// Drive the Hono app through a series of requests and print the responses.
//
// This is the in-process variant: instead of binding a port (see serve-http.js for the real
// `Lumen.serve` server) we call `app.fetch(request)` directly — the same entry point the server
// calls per connection. Handy for exercising routing without a socket. Touches ESM import from
// node_modules plus the web platform: Request, Response, Headers, and URL.

import app from './app.js';

const cases = [
  new Request('http://localhost/'),
  new Request('http://localhost/api/hello?name=lumen'),
  new Request('http://localhost/users/42'),
  new Request('http://localhost/echo', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ hi: 'there' }),
  }),
  new Request('http://localhost/does-not-exist'),
];

for (const req of cases) {
  const res = await app.fetch(req);
  const body = await res.text();
  const url = new URL(req.url);
  console.log(`${req.method} ${url.pathname}${url.search}  ->  ${res.status}`);
  const powered = res.headers.get('x-powered-by');
  if (powered) console.log(`  x-powered-by: ${powered}`);
  console.log(`  ${body.trim()}`);
}
