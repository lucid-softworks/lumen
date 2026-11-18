// Reproduce from repository root after building: node reports/overnight/v8/run.mjs
import fs from 'node:fs';
import os from 'node:os';
import {spawnSync,spawn} from 'node:child_process';
import crypto from 'node:crypto';
const out='reports/overnight/v8';
const suites=['richards','deltablue','crypto','raytrace','earley-boyer','regexp','splay','navier-stokes'];
const hashes=JSON.parse(fs.readFileSync(out+'/source-hashes.json','utf8'));
fs.mkdirSync('v8-v7',{recursive:true});
for(const [file,expected] of Object.entries(hashes)) {
 const path='v8-v7/'+file;
 if(!fs.existsSync(path)) {
  const response=await fetch('https://raw.githubusercontent.com/mozilla/arewefastyet/master/benchmarks/v8-v7/'+file);
  if(!response.ok)throw Error('Download failed: '+file);
  fs.writeFileSync(path,Buffer.from(await response.arrayBuffer()));
 }
 if(crypto.createHash('sha256').update(fs.readFileSync(path)).digest('hex')!==expected)throw Error('Source hash mismatch: '+file);
}
const driver=fs.readFileSync('v8-v7/run.js','utf8').replace(/^load\(.*$/gm,'');
for(const name of suites)fs.writeFileSync(`${out}/${name}.js`,'if (typeof print === "undefined") globalThis.print = function(s) { console.log(s); };\n'+['base',name].map(f=>fs.readFileSync(`v8-v7/${f}.js`,'utf8')).join('\n')+'\n'+driver);
const metadata={date:new Date().toISOString(),commit:spawnSync('git',['rev-parse','HEAD'],{encoding:'utf8'}).stdout.trim(),node:process.version,v8:process.versions.v8,lumen:spawnSync(process.env.LUMEN_BENCH_BIN || './target/release/lumen',['--version'],{encoding:'utf8'}).stdout.trim(),rust:spawnSync('rustc',['--version'],{encoding:'utf8'}).stdout.trim(),cpu:os.cpus()[0].model,platform:os.platform(),arch:os.arch(),kernel:os.release(),loadAtStart:os.loadavg(),timeoutMs:60000,repeats:3,source:'https://github.com/mozilla/arewefastyet/tree/master/benchmarks/v8-v7',binarySha256:crypto.createHash('sha256').update(fs.readFileSync(process.env.LUMEN_BENCH_BIN || './target/release/lumen')).digest('hex'),method:'Sequential isolated processes, alternating engine order by round; unchanged upstream warmup, timing and minimum 32 iterations. 60-second process timeout; failed/time-out workloads not repeated for that engine. Scores parsed without rounding. No CPU isolation. Suite correctness checks enabled.'};
fs.writeFileSync(`${out}/metadata.json`,JSON.stringify(metadata,null,2));
const results=[];
async function run(engine,name,round){
 const command=engine==='v8'?['node',`${out}/${name}.js`]:[process.env.LUMEN_BENCH_BIN || './target/release/lumen','--tier=jit',`${out}/${name}.js`];
 const started=performance.now();let stdout='',stderr='',timedOut=false;
 const p=spawn(command[0],command.slice(1));p.stdout.on('data',d=>stdout+=d);p.stderr.on('data',d=>stderr+=d);
 const timer=setTimeout(()=>{timedOut=true;p.kill('SIGKILL')},metadata.timeoutMs);
 const [code,signal]=await new Promise(resolve=>p.on('close',(code,signal)=>resolve([code,signal])));clearTimeout(timer);
 const match=stdout.match(/^([A-Za-z]+): ([0-9]+(?:\.[0-9]+)?)$/m);
 const score=match?Number(match[2]):null;
 const status=timedOut?'timeout':code===0&&score!==null&&stdout.includes('Score (version 7):')?'ok':'error';
 const r={engine,name,round,status,score:status==='ok'?score:null,wallMs:performance.now()-started,code,signal,stdout,stderr,command};results.push(r);
 fs.writeFileSync(`${out}/results.json`,JSON.stringify(results,null,2));
 console.log(`${engine} ${name} round ${round}: ${status}, score=${r.score}, ${(r.wallMs/1000).toFixed(1)}s`);
}
for(let round=1;round<=3;round++)for(const name of suites)for(const engine of round%2?['v8','lumen']:['lumen','v8']){
 if(results.some(r=>r.engine===engine&&r.name===name&&r.status!=='ok'))continue;
 await run(engine,name,round);
}
const median = values => values.sort((a,b)=>a-b)[Math.floor(values.length/2)];
const comparisons = suites.map(name=>{
 const samples=results.filter(r=>r.name===name);
 const allPassed=samples.length===6&&samples.every(r=>r.status==='ok');
 const lumen=allPassed?median(samples.filter(r=>r.engine==='lumen').map(r=>r.score)):null;
 const v8=allPassed?median(samples.filter(r=>r.engine==='v8').map(r=>r.score)):null;
 return {name,lumen,v8,allPassed,ratio:allPassed?v8/lumen:null};
});
const passed=comparisons.every(r=>r.allPassed);
const geometricGap=passed?Math.exp(comparisons.reduce((sum,r)=>sum+Math.log(r.ratio),0)/comparisons.length):null;
const within2x=passed&&comparisons.every(r=>r.ratio<=2);
fs.writeFileSync(out+'/summary.json',JSON.stringify({results:comparisons,geometricGap,passed,within2x},null,2));
console.log('Completed; metadata, results and summary saved. Within 2x on all workloads: '+within2x);

if (!passed) process.exitCode = 1;
