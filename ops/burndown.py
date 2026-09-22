"""Burndown of docs/tasks.yaml, reconstructed from its git history.

Tickets carry no created/closed timestamps; the board file itself is the record. Every commit that
touched docs/tasks.yaml is a snapshot, so "open tickets per milestone at each timestep" is read
straight out of `git log -- docs/tasks.yaml`, and filed/closed per day out of consecutive
snapshots (a ticket id that appears = filed; a status that becomes done = closed).

Scanning is a line-level regex over id/status/milestone - deliberately not a YAML parse, because
one snapshot of the board is ~22k lines and there are >1000 of them. Results are cached per
commit hash in $HACKRIFF_OPS/burndown-cache.json, so after the first build only new commits
are scanned.
"""
import json
import os
import re
import subprocess
import threading
import time

OPEN = ("todo", "in-progress", "blocked", "paused")
_ID = re.compile(r"^  - id: (\S+)")
_STATUS = re.compile(r"^    status: (\S+)")
_MS = re.compile(r"^    milestone: (\S+)")
_LOCK = threading.Lock()


def _sh(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=120).stdout


def scan(text):
    """{ticket_id: (milestone, status)} from one board snapshot."""
    out, tid, ms, st = {}, None, "?", "?"
    for line in text.splitlines():
        m = _ID.match(line)
        if m:
            if tid:
                out[tid] = (ms, st)
            tid, ms, st = m.group(1), "?", "?"
            continue
        if tid is None:
            continue
        m = _STATUS.match(line)
        if m:
            st = m.group(1).strip("'\"")
            continue
        m = _MS.match(line)
        if m:
            ms = m.group(1).strip("'\"")
    if tid:
        out[tid] = (ms, st)
    return out


def _summarise(recs):
    open_by, done_by = {}, {}
    for ms, st in recs.values():
        if st in OPEN:
            open_by[ms] = open_by.get(ms, 0) + 1
        elif st == "done":
            done_by[ms] = done_by.get(ms, 0) + 1
    return open_by, done_by


def build(repo, cache_path, limit=None):
    """Ordered list of snapshots: {h, t, open{ms}, done{ms}, filed[], closed[]}."""
    with _LOCK:
        try:
            cache = json.load(open(cache_path))
        except Exception:
            cache = {}
        log = _sh(["git", "log", "--format=%H %ct", "--reverse", "--first-parent", "--", "docs/tasks.yaml"], repo)
        commits = [(l.split()[0], int(l.split()[1])) for l in log.splitlines() if l.strip()]
        if limit:
            commits = commits[-limit:]
        prev = None
        changed = False
        for h, t in commits:
            if h in cache:
                prev = set(cache[h]["ids_done"]) , set(cache[h]["ids"])
                continue
            recs = scan(_sh(["git", "show", f"{h}:docs/tasks.yaml"], repo))
            open_by, done_by = _summarise(recs)
            ids = set(recs)
            ids_done = {i for i, (_, s) in recs.items() if s == "done"}
            if prev is None:
                filed, closed = [], []
            else:
                prev_done, prev_ids = prev
                filed = sorted(ids - prev_ids)
                closed = sorted(ids_done - prev_done)
            cache[h] = {"t": t, "open": open_by, "done": done_by, "filed": filed, "closed": closed,
                        "ids": sorted(ids), "ids_done": sorted(ids_done)}
            prev = ids_done, ids
            changed = True
        if changed:
            tmp = cache_path + ".tmp"
            json.dump(cache, open(tmp, "w"))
            os.replace(tmp, cache_path)
        rows = []
        for h, t in commits:
            c = cache[h]
            rows.append({"h": h[:8], "t": t, "open": c["open"], "done": c["done"],
                         "filed": len(c["filed"]), "closed": len(c["closed"])})
        return rows


def series(repo, cache_path):
    """What the chart needs: per-snapshot open-by-milestone, plus filed/closed per day."""
    rows = build(repo, cache_path)
    by_day = {}
    for r in rows:
        d = time.strftime("%Y-%m-%d", time.localtime(r["t"]))
        day = by_day.setdefault(d, {"filed": 0, "closed": 0})
        day["filed"] += r["filed"]
        day["closed"] += r["closed"]
    milestones = sorted({m for r in rows for m in r["open"]} | {m for r in rows for m in r["done"]})
    total_open = [sum(r["open"].values()) for r in rows]
    return {"rows": rows, "days": by_day, "milestones": milestones,
            "now": {"open": rows[-1]["open"] if rows else {}, "done": rows[-1]["done"] if rows else {},
                    "total_open": total_open[-1] if rows else 0}}


PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>hackriff burndown</title>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--teal:#52C2AE;--amber:#F0A542;--lav:#A395E0;--coral:#E47B68;--mono:"SFMono-Regular",Menlo,monospace}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--txt);font:13px/1.5 -apple-system,system-ui,sans-serif;padding:0 16px 24px}
.top{display:flex;align-items:center;gap:14px;padding:12px 0;border-bottom:1px solid var(--line);margin-bottom:12px}
.top b{color:var(--amber)}.top a{color:var(--amber);text-decoration:none;border:1px solid rgba(240,165,66,.4);border-radius:6px;padding:2px 9px;font-size:11.5px}
.sub{color:var(--mut);font-size:12px}
.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:12px 14px;margin-bottom:12px}
h2{margin:0 0 8px;font-size:13px;letter-spacing:.06em;text-transform:uppercase;color:var(--mut)}
svg{width:100%;height:auto;display:block}
.leg{display:flex;flex-wrap:wrap;gap:6px 14px;font-size:11.5px;color:var(--mut);margin-top:6px}
.leg i{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:5px;vertical-align:-1px}
.leg label{cursor:pointer}.leg label.off{opacity:.35}
.num{font-family:var(--mono);font-variant-numeric:tabular-nums}
.tip{position:fixed;pointer-events:none;background:var(--panel);border:1px solid var(--line);border-radius:6px;padding:6px 9px;font-size:11.5px;display:none;max-width:280px}
.stats{display:flex;flex-wrap:wrap;gap:8px 22px;font-size:12px;color:var(--mut)}.stats b{color:var(--txt);font-family:var(--mono)}
</style></head><body>
<div class=top><span>hack<b>riff</b> · burndown</span><a href="/">← dashboard</a><a href="/graph">task map ↗</a><span class=sub id=sub>loading…</span></div>
<div class=card><div class=stats id=stats></div></div>
<div class=card><h2>Open tickets over time, by milestone <span class=sub>(todo + in-progress + blocked + paused; one point per board commit; click a legend entry to hide it)</span></h2><svg id=area viewBox="0 0 1000 380"></svg><div class=leg id=leg></div></div>
<div class=card><h2>Filed vs closed per day <span class=sub>(filed = id first appears on main; closed = status became done)</span></h2><svg id=bars viewBox="0 0 1000 200"></svg></div>
<div class=tip id=tip></div>
<script>
const COL=["#52C2AE","#F0A542","#A395E0","#E47B68","#7FB3D5","#F3E3BF","#8FBF6A","#D08BC6","#6A9E8C","#C9A25E","#8595A0","#5DA6A0","#B5865A","#9AA5E0","#E0A395","#6FCF97","#C0C0C0","#FF9F6B","#88C0D0","#B48EAD"];
const $=s=>document.querySelector(s); const hidden=new Set();
let D=null;
function fmtT(t){const d=new Date(t*1000);return d.toLocaleDateString(undefined,{month:'short',day:'numeric'})+' '+d.toTimeString().slice(0,5);}
function draw(){
  const rows=D.rows, ms=D.milestones.filter(m=>!hidden.has(m));
  const W=1000,H=380,L=44,R=12,T=14,B=28; const x0=rows[0].t,x1=rows[rows.length-1].t;
  const X=t=>L+(t-x0)/Math.max(1,x1-x0)*(W-L-R);
  const totals=rows.map(r=>ms.reduce((s,m)=>s+(r.open[m]||0),0)); const ymax=Math.max(10,...totals);
  const Y=v=>T+(1-v/ymax)*(H-T-B);
  let g=''; // grid + y labels
  const step=ymax>200?50:ymax>80?20:10;
  for(let v=0;v<=ymax;v+=step){g+=`<line x1="${L}" x2="${W-R}" y1="${Y(v)}" y2="${Y(v)}" stroke="#243039" stroke-width="1"/><text x="${L-6}" y="${Y(v)+4}" fill="#8595A0" font-size="11" text-anchor="end" font-family="Menlo,monospace">${v}</text>`;}
  // x labels: each day boundary
  let day=null,lastLx=-99; for(const r of rows){const d=new Date(r.t*1000).toDateString(); if(d!==day){day=d; g+=`<line x1="${X(r.t)}" x2="${X(r.t)}" y1="${T}" y2="${H-B}" stroke="#1B252D"/>`; if(X(r.t)-lastLx<48) continue; lastLx=X(r.t); g+=`<text x="${X(r.t)+3}" y="${H-B+14}" fill="#8595A0" font-size="11">${new Date(r.t*1000).toLocaleDateString(undefined,{month:'short',day:'numeric'})}</text>`;}}
  // stacked areas, bottom-up in milestone order
  let base=rows.map(()=>0), areas='';
  ms.forEach((m,i)=>{const top=rows.map((r,k)=>base[k]+(r.open[m]||0));
    let p='M'+X(rows[0].t)+','+Y(base[0]); for(let k=0;k<rows.length;k++)p+=' L'+X(rows[k].t)+','+Y(top[k]);
    for(let k=rows.length-1;k>=0;k--)p+=' L'+X(rows[k].t)+','+Y(base[k]); p+='Z';
    const c=COL[D.milestones.indexOf(m)%COL.length];
    areas+=`<path d="${p}" fill="${c}" fill-opacity=".55" stroke="${c}" stroke-width=".8"><title>${m}</title></path>`; base=top;});
  // total line
  let tl='M'+rows.map((r,k)=>X(r.t)+','+Y(totals[k])).join(' L');
  areas+=`<path d="${tl}" fill="none" stroke="#D5DEE2" stroke-width="1.5"/>`;
  $('#area').innerHTML=g+areas+`<rect id=hit x="${L}" y="${T}" width="${W-L-R}" height="${H-T-B}" fill="transparent"/><line id=cur x1="0" x2="0" y1="${T}" y2="${H-B}" stroke="#F0A542" stroke-dasharray="3 3" style="display:none"/>`;
  const svg=$('#area'), tip=$('#tip');
  svg.onmousemove=e=>{const pt=svg.createSVGPoint();pt.x=e.clientX;pt.y=e.clientY;const p=pt.matrixTransform(svg.getScreenCTM().inverse());
    const t=x0+(p.x-L)/(W-L-R)*(x1-x0); let k=0; for(let i=0;i<rows.length;i++){if(rows[i].t<=t)k=i;} const r=rows[k];
    $('#cur').setAttribute('x1',X(r.t));$('#cur').setAttribute('x2',X(r.t));$('#cur').style.display='';
    const lines=ms.filter(m=>r.open[m]).sort((a,b)=>(r.open[b]||0)-(r.open[a]||0)).map(m=>`<span style="color:${COL[D.milestones.indexOf(m)%COL.length]}">■</span> ${m} <span class=num>${r.open[m]}</span>`).join('<br>');
    tip.innerHTML=`<b>${fmtT(r.t)}</b> · ${r.h}<br><b class=num>${totals[k]} open</b> · done ${Object.values(r.done).reduce((a,b)=>a+b,0)}<br>${lines}`;
    tip.style.display='block';tip.style.left=Math.min(e.clientX+14,window.innerWidth-300)+'px';tip.style.top=(e.clientY+12)+'px';};
  svg.onmouseleave=()=>{tip.style.display='none';$('#cur').style.display='none';};
  $('#leg').innerHTML=D.milestones.map(m=>`<label class="${hidden.has(m)?'off':''}" data-m="${m}"><i style="background:${COL[D.milestones.indexOf(m)%COL.length]}"></i>${m} <span class=num>${D.now.open[m]||0}</span></label>`).join('');
  $('#leg').querySelectorAll('label').forEach(l=>l.onclick=()=>{const m=l.dataset.m; hidden.has(m)?hidden.delete(m):hidden.add(m); draw();});
  // bars
  const days=Object.keys(D.days).sort(); const W2=1000,H2=200,B2=26,T2=10; const bw=(W2-L-R)/days.length; const vmax=Math.max(5,...days.map(d=>Math.max(D.days[d].filed,D.days[d].closed)));
  let b=''; days.forEach((d,i)=>{const f=D.days[d].filed,c=D.days[d].closed; const x=L+i*bw; const hf=f/vmax*(H2-T2-B2), hc=c/vmax*(H2-T2-B2);
    b+=`<rect x="${x+bw*0.12}" y="${H2-B2-hf}" width="${bw*0.36}" height="${hf}" fill="#E47B68" fill-opacity=".8"><title>${d}: filed ${f}</title></rect><rect x="${x+bw*0.52}" y="${H2-B2-hc}" width="${bw*0.36}" height="${hc}" fill="#52C2AE" fill-opacity=".8"><title>${d}: closed ${c}</title></rect>`;
    b+=`<text x="${x+bw/2}" y="${H2-B2+14}" fill="#8595A0" font-size="11" text-anchor="middle">${d.slice(5)}</text><text x="${x+bw*0.30}" y="${H2-B2-hf-3}" fill="#E47B68" font-size="10" text-anchor="middle" font-family="Menlo,monospace">${f||''}</text><text x="${x+bw*0.70}" y="${H2-B2-hc-3}" fill="#52C2AE" font-size="10" text-anchor="middle" font-family="Menlo,monospace">${c||''}</text>`;});
  $('#bars').innerHTML=b+`<text x="${W2-R}" y="${T2+4}" fill="#8595A0" font-size="11" text-anchor="end"><tspan fill="#E47B68">■</tspan> filed  <tspan fill="#52C2AE">■</tspan> closed</text>`;
  const last=rows[rows.length-1], first=rows[0]; const doneNow=Object.values(last.done).reduce((a,b)=>a+b,0);
  const d3=days.slice(-3).reduce((a,d)=>({f:a.f+D.days[d].filed,c:a.c+D.days[d].closed}),{f:0,c:0});
  $('#stats').innerHTML=`<span>open now <b>${D.now.total_open}</b></span><span>done <b>${doneNow}</b></span><span>last 3 days: filed <b>${d3.f}</b> · closed <b>${d3.c}</b></span><span>snapshots <b>${rows.length}</b> from ${fmtT(first.t)} to ${fmtT(last.t)}</span>`;
  $('#sub').textContent='from git history of docs/tasks.yaml · '+new Date().toTimeString().slice(0,8);
}
async function load(){try{const r=await fetch('/burndown.json');D=await r.json(); if(D.error){$('#sub').textContent=D.error;return;} if(!D.rows.length){$('#sub').textContent='no snapshots yet';return;} draw();}catch(e){$('#sub').textContent='load failed: '+e;}}
load(); setInterval(load,60000);
</script></body></html>"""
