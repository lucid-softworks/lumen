#!/usr/bin/env node
// Compare the same locked Vite application in fresh Lumen, Node and Bun processes.
// npm ci --prefix examples/vite-app && cargo build --release -p lumen-cli
// node scripts/bench-vite.mjs --bun /path/to/bun --require-parity
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { spawn, execFileSync } from 'node:child_process';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  lumen: { type: 'string' }, bun: { type: 'string' },
  runs: { type: 'string', default: '7' },
  out: { type: 'string', default: 'reports/vite-comparison/latest.json' },
  'cold-cache': { type: 'boolean', default: false },
  'require-parity': { type: 'boolean', default: false },
} });
const runs = Number(values.runs);
if (!Number.isInteger(runs) || runs < 3) throw new Error('--runs must be an integer >= 3');
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cwd = path.join(root, 'examples/vite-app');
const output = path.resolve(values.out);
const engines = {
  lumen: path.resolve(values.lumen ?? process.env.LUMEN_BIN ?? path.join(root,
    `target/release/lumen-cli${process.platform === 'win32' ? '.exe' : ''}`)),
  node: process.execPath,
  bun: values.bun ?? process.env.BUN_BIN ?? 'bun',
};
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const readVersion = binary => execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim();
const git = args => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
const report = {
  metadata: {
    date: new Date().toISOString(), commit: git(['rev-parse', 'HEAD']),
    dirty: Boolean(git(['status', '--porcelain', '--untracked-files=no'])),
    versions: Object.fromEntries(Object.entries(engines).map(([name, binary]) => [name, readVersion(binary)])),
    v8: process.versions.v8,
    vite: JSON.parse(fs.readFileSync(path.join(cwd, 'node_modules/vite/package.json'))).version,
    platform: process.platform, arch: process.arch, cpu: os.cpus()[0].model,
    loadAverage: os.loadavg(), lumenSha256: hash(fs.readFileSync(engines.lumen)),
    settings: Object.fromEntries(['LUMEN_TIER', 'LUMEN_TIER_THRESHOLD', 'LUMEN_FIBER_PROBE',
      'LUMEN_PARSE_CACHE_PROBE', 'LUMEN_PHASE_TIMINGS', 'LUMEN_FUNCTION_PROFILE', 'NODE_COMPILE_CACHE',
      'NODE_DISABLE_COMPILE_CACHE'].filter(name => process.env[name] !== undefined)
      .map(name => [name, process.env[name]])),
    inputs: Object.fromEntries(['package-lock.json', 'index.html', 'main.js'].map(name =>
      [name, hash(fs.readFileSync(path.join(cwd, name)))])),
    method: 'One excluded warmup and rotated fresh-process builds per runtime; dist removed before each build; identical installed dependencies; wall time includes startup and shutdown. Warm filesystem caches on a shared machine. Output paths and SHA256 hashes must match across every run.',
    runs, timeoutMs: 120_000,
    compileCache: values['cold-cache'] ? 'New empty NODE_COMPILE_CACHE directory for every process' : 'Runtime defaults; caches retained between processes',
  },
  results: [],
};
fs.mkdirSync(path.dirname(output), { recursive: true });
const save = () => fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
let referenceHashes;

async function run(engine, round) {
  const dist = path.join(cwd, 'dist');
  fs.rmSync(dist, { recursive: true, force: true });
  const args = ['node_modules/vite/bin/vite.js', 'build'];
  const cacheDirectory = values['cold-cache'] ? fs.mkdtempSync(path.join(os.tmpdir(), 'lumen-vite-bench-')) : undefined;
  const env = cacheDirectory ? { ...process.env, NODE_COMPILE_CACHE: cacheDirectory } : process.env;
  const start = performance.now();
  const detached = process.platform !== 'win32';
  const child = spawn(engines[engine], args, { cwd, detached, env });
  let stdout = '', stderr = '', spawnError, timedOut = false;
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  child.on('error', error => { spawnError = error.message; });
  const timer = setTimeout(() => {
    timedOut = true;
    try { if (detached) process.kill(-child.pid, 'SIGKILL'); else child.kill('SIGKILL'); }
    catch (error) { if (error.code !== 'ESRCH') stderr += String(error); }
  }, report.metadata.timeoutMs);
  const [code, signal] = await new Promise(resolve => child.on('close', (code, signal) => resolve([code, signal])));
  clearTimeout(timer);
  const wallMs = performance.now() - start;
  if (cacheDirectory) fs.rmSync(cacheDirectory, { recursive: true, force: true });
  let hashes;
  if (fs.existsSync(path.join(dist, 'index.html'))) {
    hashes = Object.fromEntries(fs.readdirSync(dist, { recursive: true })
      .filter(name => fs.statSync(path.join(dist, name)).isFile()).sort()
      .map(name => [name, hash(fs.readFileSync(path.join(dist, name)))]));
  }
  const reportedError = /^(?:Uncaught(?: \(in promise\))?|Unhandled(?:PromiseRejection| rejection)|error during build:)/mi.test(stderr);
  const ok = !spawnError && !timedOut && code === 0 && !reportedError && hashes !== undefined;
  const matching = ok && (referenceHashes === undefined || JSON.stringify(hashes) === JSON.stringify(referenceHashes));
  if (matching) referenceHashes ??= hashes;
  const result = { engine, round, status: matching ? 'ok' : 'error', wallMs, code, signal,
    timedOut, spawnError, reportedError, stdout, stderr, command: [engines[engine], ...args], hashes };
  report.results.push(result);
  save();
  console.log(`${engine} ${round ? `run ${round}` : 'warmup'}: ${result.status} ${wallMs.toFixed(1)} ms`);
  if (!matching) throw new Error(`${engine}: build failed or output differs; see ${output}`);
}

try {
  const names = Object.keys(engines);
  for (let round = 0; round <= runs; round++) {
    for (let i = 0; i < names.length; i++) await run(names[(i + round) % names.length], round);
  }
  report.summary = Object.fromEntries(names.map(name => {
    const times = report.results.filter(r => r.engine === name && r.round > 0).map(r => r.wallMs).sort((a, b) => a - b);
    const middle = Math.floor(times.length / 2);
    return [name, { medianMs: times.length % 2 ? times[middle] : (times[middle - 1] + times[middle]) / 2,
      minMs: times[0], maxMs: times.at(-1) }];
  }));
  const { lumen, node, bun } = report.summary;
  report.parity = { nodeRatio: lumen.medianMs / node.medianMs,
    bunRatio: lumen.medianMs / bun.medianMs,
    passed: lumen.medianMs <= node.medianMs && lumen.medianMs <= bun.medianMs };
  save();
  console.table(report.summary);
  console.log(`Lumen/Node: ${report.parity.nodeRatio.toFixed(3)}x; Lumen/Bun: ${report.parity.bunRatio.toFixed(3)}x`);
  console.log(`Saved ${output}`);
  if (values['require-parity'] && !report.parity.passed) process.exitCode = 1;
} catch (error) {
  report.error = String(error);
  save();
  throw error;
}
