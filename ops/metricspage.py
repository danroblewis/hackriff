"""The dashboard's /metrics page (user, 2026-09-24): code metrics over committed main.

ops/monitor.py serves PAGE at /metrics and hkpy.codemetrics.build()'s JSON at /metrics.json - built in
a child process when committed main moves (a landing), cached by sha. Every section prints the
caveat of its method from the JSON (`caveats`), so the page never states more than it measured.
"""

PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>Code metrics</title>
<script src="https://cdnjs.cloudflare.com/ajax/libs/mermaid/10.9.1/mermaid.min.js"></script>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--teal:#52C2AE;--amber:#F0A542;--lav:#A395E0;--coral:#E47B68;--mono:"SFMono-Regular",Menlo,monospace}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--txt);font:13px/1.5 -apple-system,system-ui,sans-serif}
.top{display:flex;flex-wrap:wrap;align-items:center;gap:8px 14px;padding:8px 16px;border-bottom:1px solid var(--line);background:var(--panel);position:sticky;top:0;z-index:2}
.top .nm{font-weight:600}.top .nm b{color:var(--amber)}
.top a{color:var(--mut);text-decoration:none;border:1px solid var(--line);border-radius:6px;padding:2px 9px;font-size:12px}.top a:hover{color:var(--txt)}
.sub{color:var(--dim);font:11.5px var(--mono);margin-left:auto}
.wrap{max-width:1300px;margin:0 auto;padding:12px 16px 40px}
.grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:12px;align-items:start}
@media(max-width:900px){.grid{grid-template-columns:minmax(0,1fr)}}
.card{background:var(--panel);border:1px solid var(--line);border-radius:9px;padding:10px 12px;min-width:0}
.card.wide{grid-column:1/-1}
h2{font-size:11px;text-transform:uppercase;letter-spacing:.09em;color:var(--mut);margin:0 0 8px}
.big{font:600 20px var(--mono)}.big i{font-style:normal;color:var(--dim);font-size:13px;font-weight:400}
.tw{overflow-x:auto;max-height:420px;overflow-y:auto}
table{width:100%;border-collapse:collapse;font-size:12px}
th{color:var(--mut);text-align:left;font-weight:500;position:sticky;top:0;background:var(--panel)}
td,th{padding:2px 8px 2px 0;border-top:1px solid var(--line);white-space:nowrap}
td.n,th.n{text-align:right;font-family:var(--mono)}td.p{font-family:var(--mono);white-space:normal;overflow-wrap:anywhere}
.bar{display:inline-block;height:8px;border-radius:2px;background:var(--teal);vertical-align:middle}.bar.t{background:var(--lav)}
.cav{color:var(--dim);font-size:11.5px;margin-top:8px;border-top:1px dashed var(--line);padding-top:6px}
.add{color:var(--teal)}.del{color:var(--coral)}.warn{color:var(--coral)}.ok{color:var(--teal)}
.tabs>span{border:1px solid var(--line);border-radius:10px;padding:0 8px;margin-right:4px;cursor:pointer;color:var(--mut);font-size:11.5px}.tabs>span.on{border-color:var(--amber);color:var(--txt);box-shadow:inset 0 -2px 0 var(--amber)}
svg text{fill:var(--mut);font:10.5px var(--mono)}
.mm{overflow:auto;background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:6px}.mm svg text{fill:inherit;font:inherit}
.zero{color:var(--teal)}.nz{color:var(--coral)}.spark{vertical-align:middle}
.sel span{border:1px solid var(--line);border-radius:10px;padding:0 7px;margin:0 3px 3px 0;cursor:pointer;color:var(--mut);font-size:11px;display:inline-block}.sel span.on{border-color:var(--amber);color:var(--txt)}
</style></head><body>
<div class=top><span class=nm>hack<b>riff</b> · code metrics</span><a href="/">← dashboard</a><a href="/flow">flow</a><a href="/worklog">work log</a><span class=sub id=sub>loading…</span></div>
<div class=wrap><div class=grid id=g></div></div>
<script>
const esc=s=>String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const k=n=>n==null?'—':n>=10000?(n/1000).toFixed(1)+'k':n.toLocaleString();
const card=(title,body,cav,wide)=>`<div class="card${wide?' wide':''}"><h2>${title}</h2>${body}${cav?`<div class=cav>${esc(cav)}</div>`:''}</div>`;
const table=(cols,rows)=>`<div class=tw><table><tr>${cols.map(c=>`<th class="${c[2]||''}">${c[0]}</th>`).join('')}</tr>${rows.map(r=>`<tr>${cols.map(c=>`<td class="${c[2]||''}">${c[1](r)}</td>`).join('')}</tr>`).join('')}</table></div>`;
let D=null, win='7d';
function trend(rows){
  if(!rows||rows.length<2) return `<div class=cav>${rows&&rows.length?'1 daily sample so far ('+esc(rows[0].date)+'): a line needs two.':'no samples yet.'}</div>`;
  const W=600,H=160,P=34, xs=rows.map((_,i)=>P+i*(W-2*P)/(rows.length-1));
  const mx=Math.max(...rows.map(r=>Math.max(r.product,r.test)))*1.05;
  const y=v=>H-18-(v/mx)*(H-30);
  const line=(key,col)=>`<polyline fill=none stroke="${col}" stroke-width=2 points="${rows.map((r,i)=>xs[i]+','+y(r[key])).join(' ')}"/>`;
  const cmx=Math.max(1,...rows.map(r=>((r.churn_24h||{}).added||0)+((r.churn_24h||{}).removed||0)));
  const bars=rows.map((r,i)=>{const c=r.churn_24h||{};const h=((c.added||0)+(c.removed||0))/cmx*40;return `<rect x="${xs[i]-3}" y="${H-18-h}" width=6 height="${h}" fill="#F0A54266"/>`;}).join('');
  return `<svg viewBox="0 0 ${W} ${H}" width=100%>${bars}${line('product','#52C2AE')}${line('test','#A395E0')}<text x=${P} y=12>product ${k(rows.at(-1).product)} · test ${k(rows.at(-1).test)} · bars = churn that day</text><text x=${P} y=${H-4}>${esc(rows[0].date)}</text><text x=${W-P-70} y=${H-4}>${esc(rows.at(-1).date)}</text></svg>`;
}
function render(){
  const d=D, C=d.caveats||{}, L=d.lines, t=L.total, H=d.hygiene;
  document.getElementById('sub').textContent=`${d.ref==='main'?'main':'gated base'} ${d.sha.slice(0,8)} · ${d.files} files · built ${new Date(d.built*1000).toLocaleString()} in ${d.build_s} s`;
  const share=r=>{const s=r.product+r.test;return s?`<span class=bar style="width:${40*r.product/s}px"></span><span class="bar t" style="width:${40*r.test/s}px"></span>`:'';};
  const cols=[['',r=>esc(r.name)],['product',r=>k(r.product),'n'],['test',r=>k(r.test),'n'],['files',r=>r.files,'n'],['product | test',share]];
  const lines=`<div class=big>${k(t.product)} <i>product</i> / ${k(t.test)} <i>test</i></div>`+table(cols,L.by_lang);
  const areas=table(cols,L.by_area);
  const ch=d.churn, tot=ch.totals;
  const churn=`<div class=tabs>${['24h','7d','30d'].map(w=>`<span data-w=${w} class="${w===win?'on':''}">${w} <span class=add>+${k(tot[w].added)}</span> <span class=del>-${k(tot[w].removed)}</span></span>`).join('')}</div>`+
    table([['',r=>esc(r.name)],['added',r=>`<span class=add>+${k(r.added)}</span>`,'n'],['removed',r=>`<span class=del>-${k(r.removed)}</span>`,'n']],ch.by_area[win]||[])+
    '<h2 style="margin-top:10px">15 hottest files, 7 days</h2>'+table([['file',r=>esc(r.path),'p'],['lines changed',r=>k(r.changed),'n']],ch.hottest_7d);
  const T=d.tests;
  const tests=table([['',r=>esc(r.name)],['tests',r=>r.tests,'n'],['test-seconds',r=>r.seconds,'n'],['lines',r=>k(r.lines),'n'],['s / 1k lines',r=>r.s_per_1k==null?'—':`<b>${r.s_per_1k}</b>`,'n']],T.by_area)+
    `<div class=cav>JUnit run ${esc(T.run||'none')}${T.at?' · '+new Date(T.at*1000).toLocaleString():''}</div>`;
  const largest=table([['file',r=>esc(r.path),'p'],['lines',r=>k(r.lines),'n'],['test',r=>k(r.test),'n']],d.largest);
  const fns=table([['fn',r=>esc(r.fn)+(r.test?' <i style="color:var(--dim)">(test)</i>':'')],['where',r=>esc(r.path.replace(/^crates\//,''))+':'+r.line,'p'],['lines',r=>r.lines,'n']],d.longest_fns);
  const hyg=`<div class=big>${H.unsafe} <i>unsafe</i> · ${H.todo} <i>TODO/FIXME/XXX</i> · <span class="${H.clippy.warnings?'warn':'ok'}">${H.clippy.warnings==null?'?':H.clippy.warnings}</span> <i>clippy warnings</i> · <span class="${H.quarantined?'warn':'ok'}">${H.quarantined==null?'?':H.quarantined}</span> <i>quarantined</i></div>`+
    `<div class=cav>last lint: ${esc(H.clippy.line||'not found')}</div>`+
    '<h2 style="margin-top:8px">#[ignore] by reason ('+H.ignored.length+')</h2>'+table([['reason',r=>r[0]==='(no reason given)'?`<span class=warn>${esc(r[0])}</span>`:esc(r[0]),'p'],['n',r=>r[1],'n']],H.ignored_by_reason)+
    '<h2 style="margin-top:8px">by crate / area</h2>'+table([['',r=>esc(r.name)],['unsafe',r=>r.unsafe||0,'n'],['TODO',r=>r.todo||0,'n']],H.by_area);
  document.getElementById('g').innerHTML=
    card('Lines per language',lines,C.lines)+card('Lines per crate / area',areas,'')+
    card('Churn',churn,C.churn)+card('Tests and their cost, per crate',tests,C.tests)+
    card('20 largest files',largest,'')+card('Longest Rust functions (proxy)',fns,C.outliers)+
    card('Hygiene',hyg,C.hygiene)+card('Trends',trend(d.trend),C.trends)+arch(d);
  const mm=document.getElementById('mm');
  if(mm&&window.mermaid){ try{ mermaid.initialize({startOnLoad:false,theme:'dark',securityLevel:'strict'}); mermaid.run({nodes:[mm]}); }catch(e){ mm.textContent='mermaid: '+e; } }
}
let spk='lines';
const SPK={lines:'product lines',unwrap:'unwrap/expect',pub:'pub items',undoc:'undocumented pub',unsafe:'unsafe',zstd:'zstd ratio',I:'instability',D:'distance'};
function spark(vals){ const v=vals.filter(x=>x!=null); if(v.length<2) return '<span style="color:var(--dim)">'+(v.length?'1 day':'—')+'</span>';
  const mn=Math.min(...v), mx=Math.max(...v), W=90,H=18; const pts=vals.map((x,i)=>x==null?null:[i*(W/(vals.length-1)), H-2-((x-mn)/((mx-mn)||1))*(H-4)]).filter(Boolean);
  return `<svg class=spark width=${W} height=${H}><polyline fill=none stroke="#52C2AE" stroke-width=1.5 points="${pts.map(p=>p.join(',')).join(' ')}"/></svg>`; }
function arch(d){
  const A=d.arch||{}; if(A.error) return card('Architecture',`<div class=nz>${esc(A.error)}</div>`,'',true);
  const C=A.caveats||{}, R=A.rules||{}, g=R.gpl||{};
  const z=(n,label)=>`<span class="${n?'nz':'zero'}">${n==null?'?':n}</span> <i>${label}</i>`;
  const rules=`<div class=big>${z((R.layer_violations||[]).length,'layer violations')} · ${z(g.violations,'GPL outside the plugin boundary')} · <span class="${(R.ui_dsp_suspects||[]).length?'warn':'zero'}">${(R.ui_dsp_suspects||[]).length}</span> <i>DSP-in-UI suspects</i></div>`+
    ((R.layer_violations||[]).length?'<h2 style="margin-top:8px">layer violations (normal deps)</h2>'+R.layer_violations.map(v=>`<div class="p nz">${esc(v)}</div>`).join(''):'')+
    ((R.layer_violations_dev||[]).length?'<h2 style="margin-top:8px">upward dev-dependencies (test code only)</h2>'+R.layer_violations_dev.map(v=>`<div class=p>${esc(v)}</div>`).join(''):'')+
    ((g.copyleft_with_alternative||[]).length?`<div class=cav>copyleft with a permissive alternative (not a violation): ${esc(g.copyleft_with_alternative.join(', '))}</div>`:'')+
    '<h2 style="margin-top:8px">DSP-in-UI suspects (keyword proxy - read each)</h2>'+table([['where',r=>esc(r.at.replace(/^ui\/src\//,'')),'p'],['word',r=>esc(r.word)],['line',r=>esc(r.line),'p']],R.ui_dsp_suspects||[]);
  const coup=table([['crate',r=>esc(r.name)+(r.layer==null?' <i style="color:var(--dim)">unranked</i>':'')],['Ca',r=>r.ca,'n'],['Ce',r=>r.ce,'n'],['I',r=>r.instability,'n'],['A',r=>r.abstractness,'n'],['D',r=>`<b>${r.distance}</b>`,'n']],A.coupling||[]);
  const IDK=['dyn Trait','generic fns/impls','Arc<Mutex|RwLock>','static/OnceCell/lazy','mpsc','fn build(','unsafe','unwrap()/expect()'];
  const idioms=table([['crate',r=>esc(r.name)],...IDK.map(k=>[esc(k),r=>r.idioms[k]||0,'n'])],A.per_crate||[]);
  const api=table([['crate',r=>esc(r.name)],['pub items',r=>r.pub_items,'n'],['undocumented',r=>r.undocumented?`<span class=warn>${r.undocumented}</span>`:0,'n'],['API churn 7d',r=>`<span class=add>+${(r.api_churn_7d||{}).added||0}</span> <span class=del>-${(r.api_churn_7d||{}).removed||0}</span>`,'n'],['zstd ratio',r=>r.zstd_ratio,'n']],A.per_crate||[]);
  const comp='<h2>most repetitive files (lowest ratio)</h2>'+table([['file',r=>esc(r.path),'p'],['lines',r=>r.lines,'n'],['ratio',r=>r.ratio,'n']],A.most_repetitive||[])+
    '<h2 style="margin-top:8px">least compressible</h2>'+table([['file',r=>esc(r.path),'p'],['lines',r=>r.lines,'n'],['ratio',r=>r.ratio,'n']],A.least_compressible||[]);
  const days=(d.trend||[]).filter(r=>r.crates), names=(A.per_crate||[]).map(r=>r.name);
  const sparks=`<div class=sel>${Object.entries(SPK).map(([k,v])=>`<span data-spk=${k} class="${k===spk?'on':''}">${esc(v)}</span>`).join('')}</div>`+
    table([['crate',r=>esc(r)],['per day ('+days.length+' samples)',r=>spark(days.map(x=>(x.crates[r]||{})[spk]))],['now',r=>{const v=days.length?(days.at(-1).crates[r]||{})[spk]:null;return v==null?'—':v;},'n']],names);
  return card('Architecture rules (must read 0)',rules,C.rules,true)+
    card('Crate dependency graph',`<div class=mm><pre class=mermaid id=mm>${esc(A.mermaid||'')}</pre></div>`,C.graph,true)+
    card('Coupling per crate',coup,C.coupling)+card('Idioms per crate (product code)',idioms,C.idioms)+
    card('Public API, docs and compression',api,C.api+' '+C.compression)+card('Compression outliers',comp,C.compression+' Codec this build: '+(A.codec||'?')+'.')+
    card('Per-crate trends',sparks,'One point per daily sample (metrics.jsonl); the numbers, no composite score.',true)+
    card('Not yet measured','<div class=p>Complexity (rust-code-analysis: cyclomatic, cognitive, Halstead, MI) and hotspots (churn x complexity) - part B. Duplication (jscpd), ui/src import cycles (madge) and the ui/src and py/hkpy directory graphs - part C.</div>','',true);
}
document.addEventListener('click',e=>{const s=e.target.closest('.tabs>span[data-w]'); if(s&&D){win=s.dataset.w; render();}
  const k=e.target.closest('.sel span[data-spk]'); if(k&&D){spk=k.dataset.spk; render();}});
async function load(){ try{ const r=await (await fetch('/metrics.json',{cache:'no-store'})).json();
  if(r.error){ document.getElementById('sub').textContent='error: '+r.error; if(!D) document.getElementById('g').innerHTML=card('Metrics',`<div class=warn>${esc(r.error)}</div>`,''); return; }
  D=r; render(); }catch(e){ document.getElementById('sub').textContent='error: '+e; } }
load(); setInterval(load,300000);
</script></body></html>
"""
