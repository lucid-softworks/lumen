#!/usr/bin/env node
// Compare the same locked Vite application with explicit runtime cache control.
// npm ci --prefix examples/vite-app && cargo build --release -p lumen-cli
// node scripts/bench-vite.mjs --bun /path/to/bun --require-parity
// Add --baseline /path/to/previous/lumen-cli to alternate the previous build into each round.
// --cold-cache resets both Node/Lumen and Bun runtime caches for every process.
// Add --bun-default-reference to show default-cache Bun separately alongside cold runs.
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { spawn, execFileSync } from 'node:child_process';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  lumen: { type: 'string' }, bun: { type: 'string' }, baseline: { type: 'string' },
  threshold: { type: 'string' },
  runs: { type: 'string', default: '7' },
  out: { type: 'string', default: 'reports/vite-comparison/latest.json' },
  'cold-cache': { type: 'boolean', default: false },
  'bun-default-reference': { type: 'boolean', default: false },
  'require-parity': { type: 'boolean', default: false },
} });
const runs = Number(values.runs);
if (!Number.isInteger(runs) || runs < 3) throw new Error('--runs must be an integer >= 3');
if (values.threshold !== undefined && (!/^\d+$/.test(values.threshold) || Number(values.threshold) > 0xffff_ffff)) {
  throw new Error('--threshold must be an unsigned 32-bit integer');
}
if (values['bun-default-reference'] && !values['cold-cache']) {
  throw new Error('--bun-default-reference requires --cold-cache');
}
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cwd = path.join(root, 'examples/vite-app');
const output = path.resolve(values.out);
const engines = {
  lumen: path.resolve(values.lumen ?? process.env.LUMEN_BIN ?? path.join(root,
    `target/release/lumen-cli${process.platform === 'win32' ? '.exe' : ''}`)),
  node: process.execPath,
  bun: values.bun ?? process.env.BUN_BIN ?? 'bun',
  ...(values['bun-default-reference'] ? { bunDefault: values.bun ?? process.env.BUN_BIN ?? 'bun' } : {}),
  ...(values.baseline ? { baseline: path.resolve(values.baseline) } : {}),
};
for (const [name, binary] of Object.entries(engines)) {
  if (path.isAbsolute(binary) || binary.includes(path.sep)) {
    engines[name] = fs.realpathSync(binary);
    continue;
  }
  const suffixes = process.platform === 'win32' ? ['', '.exe', '.cmd'] : [''];
  const candidates = (process.env.PATH ?? '').split(path.delimiter)
    .flatMap(dir => suffixes.map(suffix => path.join(dir, binary + suffix)));
  const found = candidates.find(file => {
    try {
      fs.accessSync(file, fs.constants.X_OK);
      return fs.statSync(file).isFile();
    } catch {
      return false;
    }
  });
  if (!found) throw new Error('Executable not found: ' + binary);
  engines[name] = fs.realpathSync(found);
}
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const readVersion = binary => execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim();
const git = args => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
function cacheState(root) {
  const entries = fs.readdirSync(root, { recursive: true })
    .map(name => fs.statSync(path.join(root, name))).filter(stat => stat.isFile());
  return { files: entries.length, bytes: entries.reduce((sum, stat) => sum + stat.size, 0) };
}
const report = {
  metadata: {
    date: new Date().toISOString(), commit: git(['rev-parse', 'HEAD']),
    dirty: Boolean(git(['status', '--porcelain', '--untracked-files=no'])),
    versions: Object.fromEntries(Object.entries(engines).map(([name, binary]) => [name, readVersion(binary)])),
    binarySha256: Object.fromEntries(Object.entries(engines).map(([name, binary]) => [name, hash(fs.readFileSync(binary))])),
    benchmarkSha256: hash(fs.readFileSync(fileURLToPath(import.meta.url))),
    v8: process.versions.v8,
    vite: JSON.parse(fs.readFileSync(path.join(cwd, 'node_modules/vite/package.json'))).version,
    platform: process.platform, arch: process.arch, cpu: os.cpus()[0].model,
    loadAverage: os.loadavg(), lumenSha256: hash(fs.readFileSync(engines.lumen)),
    ...(engines.baseline ? { baselineSha256: hash(fs.readFileSync(engines.baseline)) } : {}),
    settings: Object.fromEntries(['LUMEN_TIER', 'LUMEN_TIER_THRESHOLD', 'LUMEN_FIBER_PROBE',
      'LUMEN_PARSE_CACHE_PROBE', 'LUMEN_PHASE_TIMINGS', 'LUMEN_FUNCTION_PROFILE', 'NODE_COMPILE_CACHE',
      'NODE_DISABLE_COMPILE_CACHE', 'BUN_RUNTIME_TRANSPILER_CACHE_PATH'].filter(name => process.env[name] !== undefined)
      .map(name => [name, process.env[name]])),
    inputs: Object.fromEntries(['package-lock.json', 'index.html', 'main.js'].map(name =>
      [name, hash(fs.readFileSync(path.join(cwd, name)))])),
    method: 'One excluded warmup and rotated fresh-process builds per runtime; dist removed before each build; identical installed dependencies; wall time includes startup and shutdown. Warm filesystem caches on a shared machine. Output paths and SHA256 hashes must match across every run.',
    runs, timeoutMs: 120_000,
    candidateThreshold: values.threshold ?? null,
    compileCache: values['cold-cache'] ? 'New empty explicit runtime cache directories for every process: NODE_COMPILE_CACHE for Node/Lumen and BUN_RUNTIME_TRANSPILER_CACHE_PATH for Bun. Optional bunDefault retains its default cache as a separate reference.' : 'Separate explicit runtime cache directories per engine, primed by the excluded warmup and retained for this session.',
    cacheDocumentation: 'https://bun.sh/docs/runtime/environment-variables#runtime-transpiler-caching',
  },
  results: [],
};
fs.mkdirSync(path.dirname(output), { recursive: true });
const sessionCacheRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'lumen-explicit-cache-session-'));
const save = () => fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
let referenceHashes;

async function run(engine, round) {
  const dist = path.join(cwd, 'dist');
  fs.rmSync(dist, { recursive: true, force: true });
  const args = ['node_modules/vite/bin/vite.js', 'build'];
  const cacheDirectory = values['cold-cache'] ? fs.mkdtempSync(path.join(sessionCacheRoot, 'cold-')) : path.join(sessionCacheRoot, engine);
  fs.mkdirSync(cacheDirectory, { recursive: true });
  const cacheBefore = cacheState(cacheDirectory);
  if ((values['cold-cache'] || round === 0) && cacheBefore.files !== 0) {
    throw new Error(`${engine}: expected empty cache before first use`);
  }
  const env = { ...process.env,
    NODE_COMPILE_CACHE: path.join(cacheDirectory, 'node'),
    BUN_RUNTIME_TRANSPILER_CACHE_PATH: path.join(cacheDirectory, 'bun'),
    ...(engine === 'lumen' && values.threshold !== undefined ? { LUMEN_TIER_THRESHOLD: values.threshold } : {}),
  };
  if (engine === 'bunDefault') delete env.BUN_RUNTIME_TRANSPILER_CACHE_PATH;
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
  const cacheAfter = cacheState(cacheDirectory);
  const cacheError = engine === 'bunDefault' ? undefined
    : cacheAfter.files === 0 ? 'Runtime did not populate its explicit cache'
    : !values['cold-cache'] && round > 0 && cacheBefore.files === 0 ? 'Warm cache was empty'
    : undefined;
  if (values['cold-cache']) fs.rmSync(cacheDirectory, { recursive: true, force: true });
  let hashes;
  if (fs.existsSync(path.join(dist, 'index.html'))) {
    hashes = Object.fromEntries(fs.readdirSync(dist, { recursive: true })
      .filter(name => fs.statSync(path.join(dist, name)).isFile()).sort()
      .map(name => [name, hash(fs.readFileSync(path.join(dist, name)))]));
  }
  const reportedError = /^(?:Uncaught(?: \(in promise\))?|Unhandled(?:PromiseRejection| rejection)|error during build:)/mi.test(stderr);
  const ok = !spawnError && !timedOut && code === 0 && !reportedError && !cacheError && hashes !== undefined;
  const matching = ok && (referenceHashes === undefined || JSON.stringify(hashes) === JSON.stringify(referenceHashes));
  if (matching) referenceHashes ??= hashes;
  const result = { engine, round, cacheControl: engine === 'bunDefault' ? 'runtime default' : 'explicit',
    cacheBefore, cacheAfter, cacheError, status: matching ? 'ok' : 'error', wallMs, code, signal,
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
    ...(report.summary.bunDefault ? { bunDefaultRatio: lumen.medianMs / report.summary.bunDefault.medianMs } : {}),
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
} finally {
  fs.rmSync(sessionCacheRoot, { recursive: true, force: true });
}
