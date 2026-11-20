import fs from 'node:fs';
import path from 'node:path';
if(!process.env.LUMEN_PGO_DIR)throw Error('Run scripts/build-profiled-cli.sh to prepare a fresh training directory');
const directory=path.resolve(process.env.LUMEN_PGO_DIR);
const fixtures=path.join(directory,'training');
fs.mkdirSync(fixtures,{recursive:true});
const bench=fs.readFileSync('crates/lumen/benches/engine.rs','utf8');
const workloads=[...bench.matchAll(/const ([A-Z_]+): &str = ("(?:\\[\s\S]|[^"\\])*");/g)].map(([,name,literal])=>({name,source:JSON.parse(literal.replace(/\\\n\s*/g,''))}));
if(workloads.length!==8)throw Error('Expected all eight existing engine workloads');
const fib=workloads.find(workload=>workload.name==='FIB');
if(!fib?.source.endsWith(')(28)'))throw Error('Unexpected Fibonacci workload');
fib.source=fib.source.slice(0,-5)+')(18)';
fs.writeFileSync(path.join(fixtures,'engine.js'),workloads.map(({name,source})=>`console.log(${JSON.stringify(name)}, JSON.stringify(${source}));`).join('\n')+'\n');
const declarations=[];
for(let n=0;n<6000;n++)declarations.push(`function transform${n}(value, options = {}) { const {factor = 2} = options; const values = [value, value + ${n}]; return values.map(item => item * factor).reduce((sum, item) => sum + item, 0); }`);
fs.writeFileSync(path.join(fixtures,'parse.js'),`const source=${JSON.stringify(declarations.join('\n')+'\ntransform5999(3);')};\nlet total=0;for(let n=0;n<8;n++)total+=eval(source+'\\n// '+n);\nconsole.log('parse',total);\n`);
for(const extension of ['mjs','cjs']) {
 const imports=[];
 for(let n=0;n<12;n++){
  const functions=declarations.slice(n*500,(n+1)*500).join('\n');
  const name='part'+n+'.'+extension;
  const exported='transform'+(n*500+499);
  fs.writeFileSync(path.join(fixtures,name),functions+(extension==='mjs'?`\nexport {${exported} as transform};\n`:`\nexports.transform=${exported};\n`));
  imports.push(extension==='mjs'?`import {transform as f${n}} from './${name}';`:`const f${n}=require('./${name}').transform;`);
 }
 fs.writeFileSync(path.join(fixtures,'modules.'+extension),imports.join('\n')+`\nconsole.log('modules',${Array.from({length:12},(_,n)=>`f${n}(${n})`).join('+')});\n`);
}
fs.copyFileSync(new URL('./async-io.js',import.meta.url),path.join(fixtures,'async-io.js'));
fs.writeFileSync(path.join(directory,'training-manifest.json'),JSON.stringify({method:'Tooling-oriented mix: existing engine expressions with Fibonacci input reduced from 28 to 18; 6,000 generated functions for parsing and 12 modules with 500 functions each; asynchronous filesystem and hashing workloads. Vite, Rollup, esbuild, and locked app sources excluded.',parameters:{functionCount:6000,functionsPerModule:500,evalParses:8,fibonacciInput:18},engineWorkloads:workloads.map(w=>w.name),entries:['engine.js','parse.js','modules.cjs','modules.mjs','async-io.js']},null,2)+'\n');
