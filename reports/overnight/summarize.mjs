import fs from 'node:fs';
const dir='reports/overnight';
const read=name=>JSON.parse(fs.readFileSync(dir+'/'+name,'utf8'));
const warm=read('vite-warm-extended.json'),cold=read('vite-cold.json'),v8=read('v8/summary.json'),baseline=read('baseline/v8-summary.json');
if(!warm.results.every(r=>r.status==='ok')||!cold.results.every(r=>r.status==='ok')||!v8.passed)throw Error('A correctness check failed');
const names={'richards':'Richards','deltablue':'DeltaBlue','crypto':'Crypto','raytrace':'RayTrace','earley-boyer':'EarleyBoyer','regexp':'RegExp','splay':'Splay','navier-stokes':'NavierStokes'};
const viteChart={id:'vite-build-chart',title:'Vite build · median wall time',description:'Lower is better. Includes startup, bundling and shutdown.',height:280,spec:{type:'bar',x:'Compile cache',y:['Lumen','Node','Bun'],yLabel:'Milliseconds',showValues:true,valueFormat:{style:'decimal',maximumFractionDigits:0},colors:{Lumen:'var(--chart-1)',Node:'var(--chart-2)',Bun:'var(--chart-3)'}},columns:['Compile cache','Lumen','Node','Bun'],rows:[['Warm',...['lumen','node','bun'].map(k=>warm.summary[k].medianMs)],['Empty',...['lumen','node','bun'].map(k=>cold.summary[k].medianMs)]]};
const v8Chart={id:'v8-gap-chart',title:'V8 benchmark · remaining speed gap',description:'V8 score ÷ Lumen score. Every workload must be at or below 2× for the JIT PR.',height:350,spec:{type:'horizontalBar',y:'Workload',x:'Ratio',xLabel:'V8 / Lumen score (×)',showValues:true,valueFormat:{style:'decimal',maximumFractionDigits:1},referenceLines:[{axis:'x',value:2,label:'2× gate'}]},columns:['Workload','Ratio'],rows:v8.results.map(r=>[names[r.name],r.ratio])};
const summary={vite:{warm:warm.summary,cold:cold.summary,warmParity:warm.parity,coldParity:cold.parity},v8:{...v8,originalGeometricGap:baseline.aggregateRatio},performancePrAllowed:warm.parity.passed&&cold.parity.passed&&v8.within2x,glibcPr:'https://github.com/lucid-softworks/lumen/pull/53'};
fs.writeFileSync(dir+'/summary.json',JSON.stringify(summary,null,2)+'\n');
const safeJson=value=>JSON.stringify(value).replaceAll('<','\\u003c');
const html=fs.readFileSync(dir+'/report-template.html','utf8').replace('__VITE_CHART__',safeJson(viteChart)).replace('__V8_CHART__',safeJson(v8Chart)).replace('__V8_SUMMARY__',`All 48 runs passed. Geometric mean gap: ${v8.geometricGap.toFixed(1)}×; original baseline: ${baseline.aggregateRatio.toFixed(1)}× (separate measurement window).`);
fs.writeFileSync(dir+'/lumen-performance.html',html);
for(const chart of [viteChart,v8Chart]){if(chart.rows.some(r=>r.length!==chart.columns.length||r.slice(1).some(v=>!Number.isFinite(v))))throw Error('Invalid chart rows');}
console.log(JSON.stringify(summary,null,2));
