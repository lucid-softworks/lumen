// Start the Express app's HTTP server. `app.listen` goes through lumen's node:http, which binds
// a real socket via Lumen.serve — so `curl localhost:3000` works like any Node server.
const app = require('./app');

const port = Number(process.env.PORT || 3000);
app.listen(port, () => {
  console.log(`express on lumen: http://localhost:${port}/`);
});
