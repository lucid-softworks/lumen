import app from './app.js';
Lumen.serve(app.fetch, {
  hostname: '127.0.0.1',
  port: 3000,
  onListen: ({ port }) => console.log(`hono on lumen: http://localhost:${port}/`),
});
