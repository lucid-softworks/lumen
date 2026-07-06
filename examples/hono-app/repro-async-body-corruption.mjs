// Regression check (FIXED) for an async body-read corruption that surfaced running this example.
//
// A Hono *middleware* route (`app.use` + `await next()`) used to make the response's async body
// read (`res.text()`) resolve to `undefined` instead of its real value, and poison every LATER
// request's body read in the same realm.
//
// Root cause: an `async`/generator body runs on a coroutine, outside `Interp::call`'s tail-call
// trampoline. A leaked `tco_ok == true` (ambient state a promise reaction can leave set) made a
// top-level `return f(...)` in that body get parked as a *pending tail call* that nothing ran, so
// the function resolved to `undefined`. Fixed in crate `lumen` by forcing `tco_ok` off before
// each statement of a coroutine body (`run_async`/`run_generator`), plus saving/restoring it
// across the coroutine handoff. See the unit test `async_tail_return_survives_leaked_tco`.
//
// Run (from examples/hono-app, after `npm install`):
//   ../../target/release/lumen-cli repro-async-body-corruption.mjs
// Now matches Node:  {"t":1}  then  {"plain":true}
import { Hono } from 'hono';

const app = new Hono();
app.use('/t', async (c, next) => { await next(); });   // middleware that only awaits next()
app.get('/t', (c) => c.json({ t: 1 }));
app.get('/plain', (c) => c.json({ plain: true }));

const read = async (p) =>
  JSON.stringify(await (await app.fetch(new Request('http://localhost' + p))).text());

console.log('middleware route /t :', await read('/t'));      // -> undefined in lumen
console.log('plain route AFTER   :', await read('/plain'));  // -> undefined in lumen (poisoned)
