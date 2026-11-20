import assert from 'node:assert/strict';
import fs from 'node:fs';
import crypto from 'node:crypto';
import {execFileSync} from 'node:child_process';
const data = JSON.parse(fs.readFileSync(new URL('./measurements.json', import.meta.url)));
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const median = values => {
  const sorted = [...values].sort((a, b) => a - b);
  return (sorted[(sorted.length - 1) >> 1] + sorted[sorted.length >> 1]) / 2;
};
assert.equal(data.observations.length, 1818);
assert.equal(data.observations.filter(row => row.round > 0).length, 1800);
for (const [file, hash] of Object.entries(data.provenance.sourceHashes)) assert.equal(sha(file), hash, file);
assert.equal(sha('scripts/bench-vite.mjs'), data.provenance.benchmarkSha256);
for (const [file, hash] of Object.entries(data.provenance.inputSha256)) assert.equal(sha('examples/vite-app/' + file), hash, file);
assert.equal(execFileSync('git', ['rev-parse', 'HEAD:crates'], {encoding: 'utf8'}).trim(), data.provenance.cratesTree);
let seed = 0x19283746;
const random = () => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) / 4294967296; };
for (const mode of ['warm', 'cold']) {
  for (const [engine, expected] of Object.entries(data.modes[mode].medians)) {
    const rows = data.observations.filter(row => row.mode === mode && row.engine === engine);
    assert.equal(rows.length, 202);
    assert.equal(median(rows.filter(row => row.round > 0).map(row => row.wallMs)), expected);
    for (const row of rows) {
      assert.equal(row.status, 'ok');
      if (engine === 'bunDefault') { assert.equal(row.cacheControl, 'runtime default'); continue; }
      assert.equal(row.cacheControl, 'explicit');
      assert(row.cacheAfter.files > 0);
      if (mode === 'cold' || row.round === 0) assert.equal(row.cacheBefore.files, 0);
      else assert(row.cacheBefore.files > 0);
    }
  }
  const comparisons = [['candidateVsBaseline', 'lumen', 'baseline'], ['candidateVsNode', 'lumen', 'node'], ['candidateVsBun', 'lumen', 'bun'],
    ...(mode === 'cold' ? [['candidateVsDefaultCacheBun', 'lumen', 'bunDefault'], ['bunIsolatedVsDefault', 'bun', 'bunDefault']] : [])];
  for (const [label, numerator, denominator] of comparisons) {
    const groups = ['first', 'repeat'].map(session => {
      const rows = data.observations.filter(row => row.mode === mode && row.session === session);
      return Array.from({length: 100}, (_, i) => rows.find(row => row.engine === numerator && row.round === i + 1).wallMs / rows.find(row => row.engine === denominator && row.round === i + 1).wallMs);
    });
    const boot = Array.from({length: 10000}, () => median(groups.flatMap(group => Array.from({length: group.length}, () => group[Math.floor(random() * group.length)])))).sort((a, b) => a - b);
    assert.deepEqual({medianPairedRatio: median(groups.flat()), interval95: [boot[250], boot[9750]], sessionPairedMedians: groups.map(median)}, data.paired.modes[mode][label]);
    if (numerator === 'lumen') { assert(boot[9750] < 1); assert(groups.every(group => median(group) < 1)); }
  }
}
assert.equal(data.validation.annotatedMemcheck.errors, 0);
assert.equal(Object.values(data.validation.annotatedMemcheck.counts).reduce((a, b) => a + b, 0), 64);
console.log('Source identity, all 1,818 observations, cache conditions, medians, paired intervals and validation counts verified.');
