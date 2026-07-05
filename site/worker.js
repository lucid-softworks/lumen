// Runs the lumen engine off the main thread so a runaway script can't freeze the page —
// the page terminates this worker to stop execution and spawns a fresh one.
//
// Every eval runs in a fresh realm: the session is rebuilt right after each result is posted
// (pre-warmed, so the rebuild cost never sits in front of a run).
import init, { Session } from './pkg/lumen_wasm.js';

let session = null;

const ready = init().then(() => {
  session = new Session();
  postMessage({ type: 'ready' });
});

onmessage = async (e) => {
  await ready;
  const { type, id, src } = e.data;
  if (type === 'eval') {
    const started = performance.now();
    try {
      const result = session.eval(src);
      const ms = performance.now() - started;
      postMessage({ type: 'result', id, result, ms });
      session.free();
      session = new Session();
    } catch (err) {
      // A host-stack overflow or wasm trap can leave the instance poisoned; report it so the
      // page can respawn a fresh worker. (onmessage is async, so an uncaught throw here would
      // become an unhandled rejection and never reach Worker#onerror.)
      postMessage({ type: 'fatal', id, message: String(err && err.message || err) });
    }
  }
};
