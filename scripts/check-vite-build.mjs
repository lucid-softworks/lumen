import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';

assert(process.argv[2], 'Pass the lumen-cli binary to check');
const candidate = path.resolve(process.argv[2]);
const cwd = path.resolve('examples/vite-app');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'lumen-vite-check-'));
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
let expected;
try {
  for (const [name, binary] of [['node', process.execPath], ['lumen', candidate]]) {
    const cache = path.join(temp, name);
    for (const mode of ['cold', 'warm']) {
      fs.rmSync(path.join(cwd, 'dist'), {recursive: true, force: true});
      const run = spawnSync(binary, ['node_modules/vite/bin/vite.js', 'build'], {
        cwd, encoding: 'utf8', timeout: 120000,
        env: {...process.env, NODE_COMPILE_CACHE: cache, LUMEN_TIER: 'jit', LUMEN_TIER_THRESHOLD: '8'},
      });
      assert.equal(run.status, 0, JSON.stringify({name, mode, stdout: run.stdout, stderr: run.stderr}));
      assert(run.stdout.includes('built in'), 'Missing completed Vite build');
      assert(!/Uncaught|Unhandled|error during build:/.test(run.stderr), run.stderr);
      const dist = path.join(cwd, 'dist');
      const hashes = Object.fromEntries(fs.readdirSync(dist, {recursive: true})
        .filter(file => fs.statSync(path.join(dist, file)).isFile()).sort()
        .map(file => [file, sha(path.join(dist, file))]));
      assert(Object.keys(hashes).length > 0);
      expected ??= hashes;
      assert.deepEqual(hashes, expected, name + ' ' + mode + ' output mismatch');
      assert(fs.readdirSync(cache, {recursive: true}).some(file => fs.statSync(path.join(cache, file)).isFile()), 'Compile cache must contain entries');
      console.log(name + ' ' + mode + ': identical build output, cache populated');
    }
  }
} finally {
  fs.rmSync(temp, {recursive: true, force: true});
}
