#!/usr/bin/env python3
"""hackriff agent monitor: worktrees, diffs, tasks, coordinator pane, agent sessions."""
import json, os, re, subprocess, glob, time, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = "/Users/daniellewis/hackriff"
SCRATCH = os.environ.get("HACKRIFF_OPS", os.path.expanduser("~/.hackriff-ops"))
os.makedirs(SCRATCH, exist_ok=True)
PROJ = "/Users/daniellewis/.claude/projects/-Users-daniellewis-hackriff"

# A self-contained ticket-detail modal: any element with data-tid opens it (fetches
# /ticket.json and shows every field). Injected before </body> of any page, so a
# ticket id can be a link anywhere. Namespaced (tm-*) with its own esc, zero deps.
TICKET_MODAL = r"""
<div class=tm-backdrop id=tm-backdrop hidden></div>
<div class=tm-panel id=tm-panel hidden role=dialog aria-label="ticket detail"></div>
<style>
.tlink{cursor:pointer;color:var(--amber,#F0A542);border-bottom:1px dotted currentColor;text-decoration:none}
.tlink:hover{color:var(--txt,#fff)}
.tm-backdrop{position:fixed;inset:0;z-index:40;background:rgba(0,0,0,.55)}
.tm-panel{position:fixed;z-index:50;left:50%;top:50%;transform:translate(-50%,-50%);width:min(560px,92vw);max-height:82vh;overflow:auto;background:var(--card,#12181d);border:1px solid var(--line,#22303a);border-radius:10px;padding:12px 14px;box-shadow:0 10px 40px rgba(0,0,0,.5)}
.tm-panel h3{margin:0 0 2px;font-size:13px;color:var(--amber,#F0A542);display:flex;justify-content:space-between;align-items:center;gap:8px}
.tm-panel .sub{color:var(--mut,#8595A0);font-size:12px;margin-bottom:8px}
.tm-panel .x{cursor:pointer;color:var(--mut,#8595A0);border:1px solid var(--line,#22303a);border-radius:5px;padding:0 6px;font-size:12px;line-height:1.7}
.tm-panel .x:hover{color:var(--txt,#fff)}
.tm-panel dl{margin:0;display:grid;grid-template-columns:auto 1fr;gap:4px 10px;font-size:12px}
.tm-panel dt{color:var(--dim,#5A6973);white-space:nowrap}
.tm-panel dd{margin:0;overflow-wrap:anywhere}
.tm-panel code{font-family:ui-monospace,monospace}
.tm-panel ul{margin:2px 0;padding-left:16px}
.tm-panel{transition:width .12s ease}
.tm-panel.tm-wide{width:min(960px,95vw)}
.tm-trbtn{margin-top:12px;background:var(--bg,#0c1116);color:var(--mut,#8595A0);border:1px solid var(--line,#22303a);border-radius:6px;padding:4px 11px;font-size:12px;cursor:pointer}
.tm-trbtn:hover{color:var(--txt,#fff);border-color:var(--dim,#5A6973)}
.tm-trbody{margin-top:10px;padding:10px 2px 2px;border-top:1px solid var(--line,#22303a);font-size:12.5px;line-height:1.62;color:var(--txt,#cdd6de);max-height:52vh;overflow:auto;overflow-wrap:anywhere}
.tm-trbody b{color:var(--txt,#fff)}
.tm-trbody h4{margin:14px 0 3px;font-size:12px;font-weight:600;color:var(--amber,#F0A542);letter-spacing:.2px}
.tm-trbody pre{background:var(--bg,#0c1116);border:1px solid var(--line,#22303a);border-radius:6px;padding:7px 9px;margin:6px 0;overflow:auto;white-space:pre-wrap;font-family:ui-monospace,monospace;font-size:11px;line-height:1.45;color:#9fb2c0}
.tm-trbody code{font-family:ui-monospace,monospace;background:rgba(255,255,255,.05);color:#9fd0c0;padding:1px 4px;border-radius:4px;font-size:11px}
.tm-trbody pre .cm{color:#5A6973;font-style:italic}
.tm-trbody pre .st{color:#9ecb8a}
.tm-trbody pre .fl{color:#e0a758}
.tm-trbody pre .da{color:#8fd9a0}
.tm-trbody pre .dr{color:#e08b7a}
.tm-dim{color:var(--dim,#5A6973);font-size:11px;margin-bottom:8px}
</style>
<script>
(function(){
  const esc=s=>String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
  const P=document.getElementById('tm-panel'), B=document.getElementById('tm-backdrop');
  const fmtVal=v=>{ if(v==null) return '';
    if(Array.isArray(v)) return v.length?('<ul>'+v.map(x=>'<li>'+fmtVal(x)+'</li>').join('')+'</ul>'):'';
    if(typeof v==='object') return '<ul>'+Object.entries(v).map(([k,x])=>`<li><b>${esc(k)}</b>: ${fmtVal(x)}</li>`).join('')+'</ul>';
    return esc(String(v)); };
  const when=t=>t?new Date(t*1000).toLocaleString(undefined,{month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}):'';
  function hlCode(lang,raw){
    let c=esc(raw);
    if(lang==='diff'){ return c.split('\n').map(l=> l[0]==='+'?'<span class=da>'+l+'</span>' : l[0]==='-'?'<span class=dr>'+l+'</span>' : l).join('\n'); }
    if(lang==='bash'||lang==='sh'){
      c=c.replace(/(^|\n)(\s*#[^\n]*)/g,(m,a,b)=>a+'<span class=cm>'+b+'</span>');
      c=c.replace(/(&quot;.*?&quot;|'[^'\n]*')/g,m=>'<span class=st>'+m+'</span>');
      c=c.replace(/(^|[\s;|&(])(--?[A-Za-z][\w-]*)/g,(m,a,b)=>a+'<span class=fl>'+b+'</span>');
      return c;
    }
    return c;
  }
  function mdToHtml(md){
    const blocks=[];
    md=String(md).replace(/```([a-z]*)\n?([\s\S]*?)```/g,(m,lang,code)=>{blocks.push('<pre>'+hlCode(lang,code.replace(/\n$/,''))+'</pre>');return '%%CB'+(blocks.length-1)+'%%';});
    let h=esc(md).replace(/`([^`\n]+)`/g,'<code>$1</code>').replace(/\*\*([^*\n]+)\*\*/g,'<b>$1</b>').replace(/^#{1,6}\s?(.*)$/gm,'<h4>$1</h4>').replace(/\n/g,'<br>');
    return h.replace(/%%CB(\d+)%%(<br>)?/g,(m,i)=>blocks[+i]);
  }
  function panelHtml(d){
    const rows=[], done=new Set(['id','title','timing']);
    const add=(k,v)=>{ if(v==null||v==='') return; rows.push(`<dt>${esc(k)}</dt><dd>${v}</dd>`); };
    const render=k=>{ if(done.has(k)||d[k]==null||d[k]==='') return; done.add(k);
      if(k==='status'){ add('status',`<span class="chip ${esc(d[k])}">${esc(d[k])}</span>`); return; }
      if(k==='commit'){ add('commit',`<code>${fmtVal(d[k])}</code>`); return; }
      add(k.replace(/_/g,' '), fmtVal(d[k])); };
    ['status','milestone','priority','model','effort','parallel_group','core_interface','deps','blocked_on','area','commit','use_cases','found_by','acceptance','notes'].forEach(render);
    Object.keys(d).sort().forEach(render);
    const ti=d.timing;
    if(ti){ add('started',when(ti.start)); add('ended', ti.end?when(ti.end):'(ongoing)'); if(ti.commits!=null) add('commits',ti.commits); }
    return `<h3>${esc(d.id)}<span class=x id=tm-close title="close (Esc)">✕</span></h3><div class=sub>${esc(d.title||'')}</div><dl>${rows.join('')}</dl><div class=tm-trans><button type=button id=tm-trbtn class=tm-trbtn>▸ transcript</button><div id=tm-trbody class=tm-trbody hidden></div></div>`;
  }
  const close=()=>{ P.hidden=true; B.hidden=true; };
  async function open(id){ let d; try{ d=await (await fetch('/ticket.json?id='+encodeURIComponent(id),{cache:'no-store'})).json(); }catch(e){ return; }
    if(!d||!d.id) return; P.innerHTML=panelHtml(d); P.hidden=false; B.hidden=false;
    document.getElementById('tm-close').onclick=close;
    const trb=document.getElementById('tm-trbtn'), trbody=document.getElementById('tm-trbody');
    let trLoaded=false, trHas=false;
    async function showTr(){
      trbody.hidden=false; trb.textContent='▾ transcript';
      if(trLoaded){ if(trHas) P.classList.add('tm-wide'); return; }
      trLoaded=true; trbody.innerHTML='<div class=tm-dim>loading…</div>';
      try{
        const r=await (await fetch('/transcript.json?id='+encodeURIComponent(id),{cache:'no-store'})).json();
        if(r&&r.markdown){ trHas=true; P.classList.add('tm-wide');
          trbody.innerHTML=`<div class=tm-dim>agent transcript · ${esc(r.file||'')}${r.size_kb?' · '+r.size_kb+' KB':''}</div>`+mdToHtml(r.markdown); }
        else { trbody.innerHTML='<div class=tm-dim>no agent transcript for this ticket (the coordinator may have done it directly)</div>'; }
      }catch(e){ trbody.innerHTML='<div class=tm-dim>failed to load transcript</div>'; }
    }
    function hideTr(){ trbody.hidden=true; trb.textContent='▸ transcript'; P.classList.remove('tm-wide'); }
    if(trb) trb.onclick=()=>{ trbody.hidden ? showTr() : hideTr(); };
    showTr();   // open by default
  }
  window.openTicketModal=open;
  document.addEventListener('click',e=>{ const t=e.target.closest('[data-tid]'); if(t){ e.preventDefault(); e.stopPropagation(); open(t.getAttribute('data-tid')); } });
  B.addEventListener('click',close);
  document.addEventListener('keydown',e=>{ if(e.key==='Escape'&&!P.hidden) close(); });
})();
</script>
"""
COORD = "8300e018-dfef-49d0-b838-fcebaf0018f3"
SUPER = "573a0024"
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
def ticket_num(s):
    m = re.search(r"T-(\d+)", s or "")
    return int(m.group(1)) if m else 10 ** 9

def sh(args, cwd=None, timeout=8):
    try:
        return subprocess.run(args, cwd=cwd, capture_output=True, text=True, timeout=timeout).stdout
    except Exception as e:
        return f"(error: {e})"

def worktrees(active=None):
    out = sh(["git", "worktree", "list", "--porcelain"], cwd=REPO)
    trees, cur = [], {}
    for line in out.splitlines():
        if line.startswith("worktree "): cur = {"path": line[9:]}
        elif line.startswith("branch "): cur["branch"] = line[7:].replace("refs/heads/", "")
        elif line.startswith("HEAD "): cur["head"] = line[5:12]
        elif line == "" and cur: trees.append(cur); cur = {}
    if cur: trees.append(cur)
    res = []
    for t in trees:
        p = t["path"]
        is_main = os.path.realpath(p) == os.path.realpath(REPO)
        uncommitted = len([l for l in sh(["git", "status", "--porcelain"], cwd=p).splitlines() if l.strip()])
        log = sh(["git", "log", "-1", "--format=%h %cr|%s"], cwd=p).strip()
        commit_when, commit_msg = (log.split("|", 1) + [""])[:2] if "|" in log else (log, "")
        if is_main:
            files = [l[3:] for l in sh(["git", "status", "--porcelain"], cwd=p).splitlines() if l.strip()][:40]
            ahead = 0; summ = ""
        else:
            # everything this branch changed since it forked off main (committed + working tree)
            ahead = 0
            try:
                ahead = int(sh(["git", "rev-list", "--count", "main..HEAD"], cwd=p).strip() or 0)
            except Exception:
                ahead = 0
            names = [l for l in sh(["git", "diff", "--name-only", "main...HEAD"], cwd=p).splitlines() if l.strip()]
            ds = sh(["git", "diff", "--stat", "main...HEAD"], cwd=p).splitlines()
            summ = ds[-1].strip() if ds else ""
            files = names[:40]
        res.append({
            "path": p, "name": os.path.basename(p), "is_main": is_main,
            "branch": t.get("branch", "(detached)"), "changed": len(files),
            "ahead": ahead, "uncommitted": uncommitted,
            "files": files, "diffstat": summ, "commit_when": commit_when, "commit_msg": commit_msg,
        })
    if active is not None:
        res = [w for w in res if w["is_main"] or any(h in (w["path"] + w["branch"]) for h in active)]
    res.sort(key=lambda w: (not w["is_main"], -w["changed"]))
    return res

def tasks():
    try:
        import yaml
        d = yaml.safe_load(open(f"{REPO}/docs/tasks.yaml"))
        t = d["tasks"] if isinstance(d, dict) else d
    except Exception:
        # crude fallback without yaml
        txt = open(f"{REPO}/docs/tasks.yaml").read()
        ids = re.findall(r"- id:\s*(\S+)", txt); sts = re.findall(r"status:\s*(\S+)", txt)
        t = [{"id": i, "status": s} for i, s in zip(ids, sts)]
    counts = {}
    active = []
    status_map = {}
    for x in t:
        s = x.get("status", "?"); counts[s] = counts.get(s, 0) + 1
        status_map[x.get("id")] = s
        if s in ("in-progress", "blocked", "paused"):
            active.append({"id": x["id"], "status": s, "milestone": x.get("milestone", ""),
                           "title": str(x.get("title", ""))[:80]})
    return {"counts": counts, "active": active, "total": len(t), "status_map": status_map}

def task_graph(scope="frontier", show_done=True, show_todo=True, show_blocked=True, at=None, ms=None):
    try:
        import yaml
        d = yaml.safe_load(open(f"{REPO}/docs/tasks.yaml"))
        tl = d["tasks"] if isinstance(d, dict) else d
    except Exception as e:
        return {"mermaid": f"graph LR\n  err[\"{e}\"]", "active": 0}
    # Milestone filter (for isolating a side-project milestone in the graph). The
    # dropdown is populated from every milestone present, so `all_milestones` is
    # computed from the FULL list before the filter narrows it.
    _nm_ms = lambda m: re.sub(r"-(fix|hardening)$", "", m or "")
    all_milestones = []
    for _x in tl:
        _m = _nm_ms(_x.get("milestone"))
        if _m and _m not in all_milestones:
            all_milestones.append(_m)
    if ms:
        tl = [x for x in tl if _nm_ms(x.get("milestone")) == ms]
    tasks = {x["id"]: x for x in tl}
    deps = lambda x: x.get("deps") or x.get("depends_on") or []
    active = [x for x in tl if x.get("status") not in ("done", "cancelled")]
    # tasks with a live agent working on them right now
    running = set()
    run_label = {}
    smap = {x["id"]: x.get("status") for x in tl}
    try:
        for a in agents(smap):
            if a.get("name", "").startswith("T-") and a.get("running"):
                running.add(a["name"]); run_label[a["name"]] = a.get("label", "")
    except Exception:
        pass
    # placeholders for running tasks not yet filed in tasks.yaml
    def node_for(tid):
        if tid in tasks:
            return tasks[tid]
        lbl = run_label.get(tid, "")
        par = re.search(r"\(milestone\s+([\w]+),\s*([^)]+)\)", lbl)
        if par:
            ms, desc = par.group(1), par.group(2).strip()
        else:
            mm = re.search(r"\b(MUI|M\d+b?)\b", lbl); ms = mm.group(1) if mm else ""; desc = ""
        return {"id": tid, "status": "in-progress", "title": (f"new · {desc}"[:40]) if desc else "just launched", "milestone": ms}
    # scope universe of candidate anchors. deferred tasks are parked — they never anchor
    # the view (only appear if they lie on the path back from real work); in "all" they can.
    if scope == "all":
        cand = [x for x in tl if x.get("status") != "cancelled"]
    else:
        cand = [x for x in active if x.get("status") != "deferred"]
    def passes(x):
        s = x.get("status")
        if s == "cancelled": return False
        if s == "done": return show_done
        if s == "todo": return show_todo
        if s in ("blocked", "paused"): return show_blocked
        if s == "deferred": return scope == "all"
        return True  # in-progress, review, etc. always anchor
    anchors = {x["id"] for x in cand if passes(x)} | set(running)
    # ALWAYS keep the dependency chain leading to any anchor, whatever its status/filter
    nodes = {}
    stack = list(anchors)
    seen = set()
    while stack:
        tid = stack.pop()
        if tid in seen: continue
        seen.add(tid)
        x = node_for(tid); nodes[tid] = x
        for dp in deps(x):
            if dp not in seen:
                stack.append(dp)
    cls = {"in-progress": "inprog", "todo": "todo", "blocked": "blocked", "paused": "blocked",
           "review": "review", "deferred": "deferred", "done": "done", "cancelled": "done"}
    def label(x):
        done = x.get("status") in ("done", "cancelled")
        t = str(x.get("title", "")).translate(str.maketrans("", "", '"[]<>|`{}')).strip()[:24]
        tick = "✓ " if done else ""
        ms = x.get("milestone") or ""
        sub = " · ".join(p for p in (ms, t) if p)
        return f"{tick}{x['id']}<br/><span style='font-size:9px;opacity:.75'>{sub}</span>" if sub else f"{tick}{x['id']}"
    lines = ["graph LR",
             "classDef done fill:#111c18,stroke:#2f5d4e,color:#6a9e8c,stroke-dasharray:4 3;",
             "classDef inprog fill:#2a2410,stroke:#F0A542,color:#F0A542,stroke-width:2px;",
             "classDef running fill:#3a2c0a,stroke:#FFCf6b,color:#FFD98a,stroke-width:3px;",
             "classDef todo fill:#1c1830,stroke:#A395E0,color:#A395E0;",
             "classDef blocked fill:#2a1512,stroke:#E47B68,color:#E47B68,stroke-width:2px;",
             "classDef review fill:#10222a,stroke:#52C2AE,color:#8FD9C9;",
             "classDef deferred fill:#15191c,stroke:#5A6973,color:#8595A0;",
             "classDef msdone fill:#123a2c,stroke:#52C2AE,color:#8FD9C9,stroke-width:2px;",
             "classDef mscur fill:#3a2c0a,stroke:#F0A542,color:#FFD98a,stroke-width:3px;",
             "classDef msnext fill:#181f24,stroke:#5A6973,color:#8595A0,stroke-dasharray:5 4;"]
    # milestone backbone: the roadmap chain, coloured by how far along each milestone is
    norm_ms = lambda m: re.sub(r"-(fix|hardening)$", "", m or "")   # fold M2-hardening/M1-fix into their base
    by_ms = {}
    for x in tl:
        by_ms.setdefault(norm_ms(x.get("milestone")), []).append(x)
    # Roadmap order first, then any other milestone that actually has tickets
    # (MCANVAS, MAUTO, MJETSON, CI, DOCS…) appended in first-seen order, so no
    # present milestone is ever dropped from the backbone and new ones auto-appear.
    ROADMAP = ["M0", "M0b", "M1", "M2", "MUI", "M3", "MCANVAS", "M4", "M5", "MAUTO", "MJETSON", "M6", "M7"]
    present = [m for m in by_ms if m]
    ORDER = [m for m in ROADMAP if m in present] + [m for m in present if m not in ROADMAP]
    def ms_status(ms):
        ts = by_ms.get(ms, [])
        if not ts:
            return "next"
        # a milestone is done when nothing is left todo/in-progress/blocked/review;
        # deferred (parked hardware) and cancelled don't hold it open.
        open_states = {"todo", "in-progress", "blocked", "paused", "review"}
        return "cur" if any(t.get("status") in open_states for t in ts) else "done"
    mscls = {"done": "msdone", "cur": "mscur", "next": "msnext"}
    for ms in ORDER:
        ts = by_ms.get(ms, [])
        d_ = sum(1 for t in ts if t.get("status") in ("done", "cancelled"))
        cnt = f"{d_}/{len(ts)}" if ts else "planned"
        lines.append(f'MS_{ms}(["{ms} · {cnt}"]):::{mscls[ms_status(ms)]}')
    chain = [m for m in ORDER if m != "MUI"]
    for a, b in zip(chain, chain[1:]):
        lines.append(f"MS_{a} --> MS_{b}")
    lines.append("MS_M1 --> MS_MUI")
    for nid, x in nodes.items():
        c = "running" if nid in running else cls.get(x.get("status"), "done")
        lines.append(f'{nid}["{label(x)}"]:::{c}')
    # dependency edges (solid) — draw among all nodes in scope, not just from active tasks
    for nid, x in nodes.items():
        for dp in deps(x):
            if dp in nodes:
                lines.append(f"{dp} --> {nid}")
    # membership links (dotted) — connect every task to its milestone node
    for nid, x in nodes.items():
        ms = norm_ms(x.get("milestone"))
        if ms in ORDER:
            lines.append(f"MS_{ms} -.-> {nid}")
    total = len(tl)
    done = sum(1 for x in tl if x.get("status") in ("done", "cancelled"))
    return {"mermaid": "\n".join(lines), "active": len(active), "nodes": len(nodes),
            "total": total, "done": done}

GRAPH_PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>hackriff task map</title>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--amber:#F0A542}
*{box-sizing:border-box}html,body{height:100%;margin:0}body{background:var(--bg);color:var(--txt);font:13px -apple-system,system-ui,sans-serif;display:flex;flex-direction:column}
.top{display:flex;align-items:center;gap:14px;padding:8px 14px;border-bottom:1px solid var(--line);background:var(--panel);flex:0 0 auto}
.top b{color:var(--amber)}a{color:var(--mut);text-decoration:none;border:1px solid var(--line);border-radius:6px;padding:3px 9px;font-size:12px}
a:hover{color:var(--txt)}.sub{color:var(--dim);font:12px ui-monospace,monospace}
.scopes{display:flex;gap:2px;background:var(--bg);border:1px solid var(--line);border-radius:7px;padding:2px}
.scopes button{border:0;background:transparent;color:var(--mut);padding:3px 10px;border-radius:5px;cursor:pointer;font-size:12px}
.scopes button.on{background:#1E2A33;color:var(--txt)}
.filters{display:flex;gap:2px;align-items:center;color:var(--dim);font-size:12px;background:var(--bg);border:1px solid var(--line);border-radius:7px;padding:2px 6px 2px 8px}
.filters button{border:1px solid var(--line);background:transparent;color:var(--dim);padding:2px 8px;border-radius:5px;cursor:pointer;font-size:12px;margin-left:2px}
.filters button.on{color:var(--txt);border-color:var(--dim);background:#1E2A33}
.filters select{background:var(--bg);color:var(--txt);border:1px solid var(--line);border-radius:5px;font-size:12px;padding:2px 4px;margin-left:4px;cursor:pointer}
.legend{margin-left:auto;display:flex;gap:10px;font-size:11px;color:var(--dim);flex-wrap:wrap}
.legend i{display:inline-block;width:9px;height:9px;border-radius:2px;margin-right:4px;vertical-align:0}
.wrap{flex:1;min-height:0;overflow:hidden;position:relative;cursor:grab;touch-action:none}
.wrap.grabbing{cursor:grabbing}
#g{position:absolute;inset:0}
#g svg{width:100%;height:100%;max-width:none;display:block}
#g .node{cursor:pointer}
#g .node:hover rect,#g .node:hover polygon{filter:brightness(1.25)}
@keyframes pulse{0%,100%{filter:drop-shadow(0 0 0 rgba(255,207,107,0))}50%{filter:drop-shadow(0 0 6px rgba(255,207,107,.8))}}
#g .running rect,#g .running polygon{animation:pulse 1.5s ease-in-out infinite}
@media(prefers-reduced-motion:reduce){#g .running rect,#g .running polygon{animation:none}}
.hint{position:fixed;bottom:10px;right:12px;color:var(--dim);font-size:11px}
*{scrollbar-width:thin;scrollbar-color:transparent transparent}
::-webkit-scrollbar{width:8px;height:8px}::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:transparent;border-radius:4px}
:hover::-webkit-scrollbar-thumb{background:rgba(133,149,160,.4)}::-webkit-scrollbar-thumb:hover{background:rgba(133,149,160,.7)}
:hover{scrollbar-color:rgba(133,149,160,.4) transparent}
</style></head><body>
<div class=top><span>hack<b>riff</b> task map</span><span class=sub id=sub></span>
<span class=scopes><button id=sc-frontier class=on>frontier</button><button id=sc-all>all tasks</button></span>
<span class=filters>show: <button id=f-done>done</button><button id=f-todo>todo</button><button id=f-blocked class=on>blocked</button></span>
<span class=filters>milestone: <select id=msfilter><option value="">all milestones</option></select></span>
<a href="/">← dashboard</a>
<span class=legend><span><i style="background:#FFD98a"></i>working now</span><span><i style="background:#F0A542"></i>in progress</span><span><i style="background:#A395E0"></i>todo</span><span><i style="background:#E47B68"></i>blocked</span><span><i style="background:#52C2AE"></i>review</span><span><i style="background:#2f5d4e"></i>✓ done</span><span><i style="background:#5A6973"></i>deferred</span></span></div>
<div class=wrap><div id=g></div></div>
<div class=hint>scroll = zoom · drag = pan · click a ticket for details · solid arrow = prerequisite → task · dotted = milestone → its tasks</div>
<script src="https://cdnjs.cloudflare.com/ajax/libs/mermaid/10.9.1/mermaid.min.js"></script>
<script>
mermaid.initialize({startOnLoad:false,theme:'dark',securityLevel:'loose',flowchart:{curve:'basis',htmlLabels:true,nodeSpacing:34,rankSpacing:70},themeVariables:{fontSize:'13px',lineColor:'#5A6973'}});
let last='',scope='frontier',flt={done:false,todo:false,blocked:true},msFilter='';
document.getElementById('sc-frontier').onclick=()=>setScope('frontier');
document.getElementById('sc-all').onclick=()=>setScope('all');
function setScope(s){scope=s;document.getElementById('sc-frontier').classList.toggle('on',s==='frontier');document.getElementById('sc-all').classList.toggle('on',s==='all');last='';draw();}
['done','todo','blocked'].forEach(k=>{document.getElementById('f-'+k).onclick=()=>{flt[k]=!flt[k];document.getElementById('f-'+k).classList.toggle('on',flt[k]);last='';draw();};});
async function draw(){
 try{
  let q='/graph.json?scope='+scope; ['done','todo','blocked'].forEach(k=>{ if(!flt[k]) q+='&'+k+'=0'; });
  const d=await (await fetch(q,{cache:'no-store'})).json();
  const hid=['done','todo','blocked'].filter(k=>!flt[k]);
  document.getElementById('sub').textContent=`${d.active} active · ${d.done}/${d.total} done · ${scope==='all'?'all tasks':'frontier'}${hid.length?' · hiding '+hid.join('/'):''}`;
  if(d.mermaid===last) return; last=d.mermaid;
  const {svg}=await mermaid.render('gg'+Date.now(), d.mermaid);
  document.getElementById('g').innerHTML=svg;
  wireNodes();
 }catch(e){ document.getElementById('g').textContent='render error: '+e; }
}
// --- SVG viewBox pan / zoom / click-to-open (vector-crisp at any zoom) ---
const wrap=document.querySelector('.wrap'), gg=document.getElementById('g');
let svgEl=null,W=0,H=0,vb=null,down=false,px=0,py=0,dragMoved=false;
function setVB(){ if(svgEl&&vb) svgEl.setAttribute('viewBox', vb.x+' '+vb.y+' '+vb.w+' '+vb.h); }
function wireNodes(){
  svgEl=gg.querySelector('svg'); if(!svgEl) return;
  const bb=svgEl.viewBox&&svgEl.viewBox.baseVal;
  W=(bb&&bb.width)||svgEl.getBBox().width; H=(bb&&bb.height)||svgEl.getBBox().height;
  svgEl.removeAttribute('width'); svgEl.removeAttribute('height');
  svgEl.setAttribute('preserveAspectRatio','xMidYMid meet');
  if(!vb) vb={x:(bb&&bb.x)||0,y:(bb&&bb.y)||0,w:W||1000,h:H||800};   // initial: fit whole graph, crisp
  setVB();
  gg.querySelectorAll('.node').forEach(n=>{ const m=(n.textContent||'').match(/T-\d+/); if(m) n.setAttribute('data-node-tid',m[0]); });
}
wrap.addEventListener('wheel',e=>{ e.preventDefault(); if(!vb)return;
  const r=wrap.getBoundingClientRect(), fx=(e.clientX-r.left)/r.width, fy=(e.clientY-r.top)/r.height;
  const nw=Math.min(W*3,Math.max(W*0.012,vb.w*Math.exp(e.deltaY*0.0015))), nh=nw*(vb.h/vb.w);
  vb.x=(vb.x+fx*vb.w)-fx*nw; vb.y=(vb.y+fy*vb.h)-fy*nh; vb.w=nw; vb.h=nh; setVB();
},{passive:false});
wrap.addEventListener('pointerdown',e=>{ down=true; dragMoved=false; px=e.clientX; py=e.clientY; });
wrap.addEventListener('pointermove',e=>{ if(!down||!vb)return; const r=wrap.getBoundingClientRect(), dx=e.clientX-px, dy=e.clientY-py;
  if(!dragMoved&&Math.abs(dx)+Math.abs(dy)>3){ dragMoved=true; wrap.classList.add('grabbing'); try{wrap.setPointerCapture(e.pointerId);}catch(_){} }
  if(dragMoved){ vb.x-=dx*(vb.w/r.width); vb.y-=dy*(vb.h/r.height); px=e.clientX; py=e.clientY; setVB(); } });
function endDrag(e){ if(down&&!dragMoved){ const n=e.target.closest('[data-node-tid]'); if(n&&window.openTicketModal) window.openTicketModal(n.getAttribute('data-node-tid')); }
  down=false; dragMoved=false; wrap.classList.remove('grabbing'); try{wrap.releasePointerCapture(e.pointerId);}catch(_){} }
wrap.addEventListener('pointerup',endDrag);
wrap.addEventListener('pointercancel',()=>{ down=false; dragMoved=false; wrap.classList.remove('grabbing'); });
draw(); setInterval(draw,15000);
</script></body></html>"""

TERM_PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>coordinator terminal</title>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--amber:#F0A542}
*{box-sizing:border-box}html,body{height:100%;margin:0}body{background:var(--bg);color:var(--txt);font:13px -apple-system,system-ui,sans-serif;display:flex;flex-direction:column}
.top{display:flex;align-items:center;gap:14px;padding:8px 14px;border-bottom:1px solid var(--line);background:var(--panel);flex:0 0 auto}
.top b{color:var(--amber)}a{color:var(--mut);text-decoration:none;border:1px solid var(--line);border-radius:6px;padding:3px 9px;font-size:12px}
a:hover{color:var(--txt)}.sub{color:var(--dim);font:12px ui-monospace,monospace;margin-left:auto}
pre{flex:1;min-height:0;overflow:auto;margin:0;padding:14px;font:11.5px/1.5 ui-monospace,Menlo,monospace;white-space:pre-wrap;word-break:break-word;color:var(--mut)}
*{scrollbar-width:thin;scrollbar-color:transparent transparent}
::-webkit-scrollbar{width:8px;height:8px}::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:transparent;border-radius:4px}
:hover::-webkit-scrollbar-thumb{background:rgba(133,149,160,.4)}::-webkit-scrollbar-thumb:hover{background:rgba(133,149,160,.7)}
:hover{scrollbar-color:rgba(133,149,160,.4) transparent}
</style></head><body>
<div class=top><span>hack<b>riff</b> · coordinator terminal <span style="color:var(--dim)">tmux “dev”</span></span><a href="/">← dashboard</a><a href="/graph">task map ↗</a><span class=sub id=sub></span></div>
<pre id=pane>…</pre>
<script>
async function tick(){try{const d=await (await fetch('/term.json',{cache:'no-store'})).json();document.getElementById('pane').textContent=d.coord||'(no pane)';document.getElementById('sub').textContent=d.now;}catch(e){document.getElementById('sub').textContent='err '+e;}}
tick(); setInterval(tick,3000);
</script></body></html>"""

def term_pane():
    raw = sh(["tmux", "capture-pane", "-pt", "dev", "-S", "-120"])
    lines = [ANSI.sub("", l).rstrip() for l in raw.splitlines()]
    lines = [l for l in lines if l.strip()]
    return "\n".join(lines[-60:])

def stage_status():
    lines = []
    try:
        with open(os.path.join(SCRATCH, "stage.log")) as f:
            lines = [l.rstrip() for l in f.read().splitlines() if l.strip()][-16:]
    except Exception:
        pass
    try:
        alive = bool(subprocess.run(["pgrep", "-f", "stage.sh"], capture_output=True, text=True, timeout=3).stdout.strip())
    except Exception:
        alive = False
    def rd(n):
        try:
            return open(os.path.join(SCRATCH, n)).read().strip()
        except Exception:
            return ""
    smoke = ""
    for l in reversed(lines):
        if "SMOKE OK" in l: smoke = "ok"; break
        if "SMOKE FAIL" in l: smoke = "fail"; break
    return {"alive": alive, "source": rd("hk-serve-source").replace("source: ", ""),
            "built": rd("hk-serve-built-commit"), "smoke": smoke, "lines": lines}

def merge_status():
    """Is the coordinator handling the merge queue right now? Distinguishes a
    coordinator that is mid-merge/gating (busy, expected zero builder agents)
    from one that is genuinely idle/stuck. Cheap: two file stats + one ps.

    Returns {state, ticket, msg, gate, elapsed_s, queue}:
      state 'merging'  a merge is staged (.git/MERGE_HEAD) and being committed
      state 'gating'   a gate/test/build process is running (pre-merge or check)
      state 'idle'     neither -> if also no builder agents, that's the stuck signal
    """
    # 1. staged merge (git merge --no-ff --no-commit, mid gate-merge)
    merging = os.path.exists(os.path.join(REPO, ".git", "MERGE_HEAD"))
    mmsg = ""; mticket = ""
    if merging:
        try:
            mmsg = open(os.path.join(REPO, ".git", "MERGE_MSG")).read().strip().splitlines()[0][:100]
        except Exception:
            mmsg = ""
        mt = re.search(r"t(?:ask-t)?0*(\d+)", mmsg, re.IGNORECASE)
        if mt:
            mticket = "T-" + mt.group(1)
    # 2. running gate / test / build process, with elapsed time and which suite
    gate = ""; elapsed = 0
    try:
        out = subprocess.run(
            ["ps", "-axo", "etimes=,command="], capture_output=True, text=True, timeout=4).stdout
        pat = re.compile(r"just (gate-merge|gate|test-ui-e2e|test-ui|test|acceptance\S*|lint\S*)"
                         r"|cargo[- ]nextest|cargo (test|build)")
        best = None
        for line in out.splitlines():
            line = line.strip()
            if " grep " in line or "pgrep" in line:
                continue
            m = pat.search(line)
            if not m:
                continue
            try:
                secs = int(line.split(None, 1)[0])
            except Exception:
                continue
            label = m.group(0).replace("cargo-nextest", "cargo nextest")
            # prefer the outermost/longest-running just-gate* over its child cargo runs
            rank = (0 if label.startswith("just gate") else 1, -secs)
            if best is None or rank < best[0]:
                best = (rank, label, secs)
        if best:
            gate, elapsed = best[1], best[2]
    except Exception:
        pass
    # 3. queue depth: in-progress tickets that aren't merged yet (rough)
    queue = 0
    try:
        import yaml
        d = yaml.safe_load(open(f"{REPO}/docs/tasks.yaml"))
        queue = sum(1 for t in (d.get("tasks") or []) if t.get("status") == "in-progress")
    except Exception:
        pass
    state = "merging" if merging else ("gating" if gate else "idle")
    return {"state": state, "msg": mmsg, "ticket": mticket,
            "gate": gate, "elapsed_s": elapsed, "queue": queue}

_PRI_RANK = {"high": 0, "medium": 1, "normal": 2, "low": 3}

def work_queue(smap, wts, ags, merge_ticket=""):
    """Two queues the dashboard couldn't show before:
      merge_ready  in-progress tickets whose branch has commits AHEAD of main and
                   are waiting their turn through the serial merge gate (the one
                   named in MERGE_MSG is flagged 'merging'). Order = merging first.
      upcoming     todo tickets in the order the coordinator will most likely take
                   them, DERIVED (not authoritative): dependency-ready first, then
                   priority (high>medium>normal>low), then ticket number. The
                   coordinator still re-orders for batching and user directives, so
                   this is a best-effort projection, labelled as such in the UI.
    """
    all_tasks = load_tasks_yaml()
    by_id = {t.get("id"): t for t in all_tasks}
    ahead_by_task = {w.get("task"): w.get("ahead", 0) for w in wts if w.get("task")}
    live_builders = {a["name"] for a in ags if a.get("running") and str(a.get("name", "")).startswith("T-")}

    # "Active milestones": where the coordinator is working right now. Retrodiction on
    # git history shows the next pick lands in an active milestone far more often than a
    # global priority sort assumes, so boosting these ~doubles the card's top-1 accuracy
    # (8%->17%) and lifts top-3 to ~45%. Active = milestones of in-progress tickets plus
    # the last ~10 merges.
    active_ms = {t.get("milestone") for t in all_tasks
                 if t.get("status") == "in-progress" and t.get("milestone")}
    try:
        recent = sh(["git", "log", "-14", "--merges", "--format=%s"], cwd=REPO, timeout=6)
        for subj in recent.splitlines():
            m = _TICKET_RE.search(subj)
            if m:
                mid = "T-" + m.group(1)
                if mid in by_id and by_id[mid].get("milestone"):
                    active_ms.add(by_id[mid]["milestone"])
    except Exception:
        pass

    # --- merge-ready: in-progress with commits on the branch ---
    merge_ready = []
    for t in all_tasks:
        if t.get("status") != "in-progress":
            continue
        tid = t.get("id")
        ahead = ahead_by_task.get(tid, 0)
        merge_ready.append({
            "id": tid, "title": str(t.get("title", ""))[:70], "ahead": ahead,
            "merging": tid == merge_ticket,
            "building": tid in live_builders and ahead == 0,
        })
    # merging first, then ready branches (most commits first), then still-building
    merge_ready.sort(key=lambda r: (not r["merging"], r["ahead"] == 0, -r["ahead"], ticket_num(r["id"])))

    # --- upcoming: todo, dependency-ready then priority then number ---
    def dep_state(t):
        # a dep blocks only while it is still open; done/cancelled/unknown don't block
        deps = t.get("deps") or []
        return [d for d in deps if smap.get(d) in ("todo", "in-progress", "blocked", "paused", "deferred")]
    upcoming = []
    for t in all_tasks:
        if t.get("status") != "todo":
            continue
        unmet = dep_state(t)
        mstone = t.get("milestone", "")
        upcoming.append({
            "id": t.get("id"), "title": str(t.get("title", ""))[:70],
            "priority": t.get("priority", "normal"), "milestone": mstone,
            "model": t.get("model", ""), "ready": not unmet,
            "waiting_on": unmet[:3], "active_ms": mstone in active_ms,
            "user": bool(t.get("requested_by") or t.get("user_report") or t.get("user_detail_2026_09_16")),
        })
    upcoming.sort(key=lambda r: (
        not r["user"],                         # user-requested first (prioritise-user-visible-fixes)
        not r["active_ms"],                    # milestones the coordinator is in now (empirically the strongest signal)
        not r["ready"],                        # ready-to-start before dep-blocked
        _PRI_RANK.get(r["priority"], 2),       # priority within the above
        ticket_num(r["id"]),
    ))
    return {"merge_ready": merge_ready, "upcoming": upcoming[:14],
            "active_ms": sorted(m for m in active_ms if m),
            "todo_total": sum(1 for t in all_tasks if t.get("status") == "todo")}

USAGE_FILE = os.path.join(SCRATCH, "usage.json")
USAGE_SESSION = "usagepoll"

def _parse_usage(text):
    """Parse the `/usage` TUI panel. Sections are headed by a label line, with the
    percentage on a following line ('  █████  91% used') and a 'Resets ...' line."""
    lines = [ANSI.sub("", l).rstrip() for l in text.splitlines()]
    def field(header):
        pct = rst = None
        for i, l in enumerate(lines):
            if header in l:
                for j in range(i + 1, min(i + 5, len(lines))):
                    if pct is None:
                        m = re.search(r"(\d+)%\s*used", lines[j])
                        if m: pct = int(m.group(1))
                    if rst is None:
                        m = re.search(r"Resets\s+(.+?)\s*(?:\(|$)", lines[j])
                        if m: rst = m.group(1).strip()
                break
        return pct, rst
    ws, wr = field("Current week (all models)")
    ss, sr = field("Current session")
    return {"weekly": ws, "weekly_reset": wr, "session": ss, "session_reset": sr}

_USAGE_LOCK = None

def _usage_lock():
    global _USAGE_LOCK
    if _USAGE_LOCK is None:
        import threading
        _USAGE_LOCK = threading.Lock()
    return _USAGE_LOCK

def poll_usage():
    """Spawn a throwaway Claude session, run /usage (no API call, $0), parse the
    panel, and write usage.json. Best-effort; always tears the session down. A lock
    serialises the auto-poller and any manual /pollbudget so they can't collide on
    the shared tmux session name."""
    lock = _usage_lock()
    if not lock.acquire(blocking=False):
        return {"skipped": "poll already running"}
    try:
        return _poll_usage_locked()
    finally:
        lock.release()

def _poll_usage_locked():
    try:
        subprocess.run(["tmux", "kill-session", "-t", USAGE_SESSION], capture_output=True, timeout=8)
    except Exception:
        pass
    try:
        subprocess.run(["tmux", "new-session", "-d", "-s", USAGE_SESSION, "-x", "200", "-y", "50",
                        "-c", REPO,
                        "claude --model claude-haiku-4-5-20251001 --dangerously-skip-permissions"],
                       capture_output=True, timeout=12)
        time.sleep(16)                                   # session boot
        subprocess.run(["tmux", "send-keys", "-t", USAGE_SESSION, "-l", "/usage"], capture_output=True, timeout=8)
        time.sleep(0.8)
        subprocess.run(["tmux", "send-keys", "-t", USAGE_SESSION, "Enter"], capture_output=True, timeout=8)
        time.sleep(4)
        cap = subprocess.run(["tmux", "capture-pane", "-pt", USAGE_SESSION, "-S", "-45"],
                             capture_output=True, text=True, timeout=8).stdout
        u = _parse_usage(cap)
        if u.get("weekly") is not None or u.get("session") is not None:
            u["updated"] = time.time(); u["auto"] = True
            json.dump(u, open(USAGE_FILE, "w"))
            return u
        return {"error": "parse failed", "raw_tail": cap[-200:]}
    except Exception as e:
        return {"error": str(e)}
    finally:
        try:
            subprocess.run(["tmux", "kill-session", "-t", USAGE_SESSION], capture_output=True, timeout=8)
        except Exception:
            pass

def _usage_poller():
    while True:
        try:
            poll_usage()
        except Exception:
            pass
        time.sleep(1200)   # every 20 min; /usage is $0 so cost is only a ~20s haiku boot

def budget_status():
    """Claude token-budget utilisation (weekly / session %). These come from the
    API's rate-limit headers, shown only by the interactive /usage command — there
    is no local file or CLI to poll them — so this tile is FED: whoever runs /usage
    drops the numbers via GET /budget?weekly=..&session=.. and the tile shows them
    with an 'as of' age so a stale value is never mistaken for live."""
    try:
        u = json.load(open(USAGE_FILE))
        age = time.time() - float(u.get("updated", 0))
        return {"weekly": u.get("weekly"), "session": u.get("session"),
                "weekly_reset": u.get("weekly_reset"), "session_reset": u.get("session_reset"),
                "auto": bool(u.get("auto")), "age_s": int(age)}
    except Exception:
        return {"weekly": None, "session": None, "age_s": None}

def git_log():
    out = sh(["git", "log", "-8", "--format=%h|%cr|%s"], cwd=REPO)
    rows = []
    for l in out.splitlines():
        p = l.split("|", 2)
        if len(p) == 3: rows.append({"h": p[0], "when": p[1], "msg": p[2][:90]})
    return rows

def coord_pane():
    raw = sh(["tmux", "capture-pane", "-pt", "dev", "-S", "-40"])
    lines = [ANSI.sub("", l).rstrip() for l in raw.splitlines()]
    lines = [l for l in lines if l.strip()]
    return "\n".join(lines[-18:])

def head_tail(path, head_n=20000, tail_n=200000):
    sz = os.path.getsize(path)
    with open(path, "rb") as f:
        head = f.read(head_n).decode("utf-8", "replace")
        if sz > tail_n:
            f.seek(sz - tail_n); tail = f.read().decode("utf-8", "replace")
        else:
            f.seek(0); tail = f.read().decode("utf-8", "replace")
    return head, tail, sz

def ts_of(line):
    m = re.search(r'"timestamp":"([^"]+)"', line)
    return m.group(1) if m else None

def parse_dt(s):
    try:
        from datetime import datetime
        return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except Exception:
        return None

def last_text(tail):
    for line in reversed(tail.splitlines()):
        try:
            d = json.loads(line)
        except Exception:
            continue
        if d.get("type") != "assistant": continue
        c = d.get("message", {}).get("content")
        if isinstance(c, list):
            for x in reversed(c):
                if isinstance(x, dict) and x.get("type") == "text" and x.get("text", "").strip():
                    return x["text"].strip()[:280]
                if isinstance(x, dict) and x.get("type") == "tool_use":
                    return "▷ " + x.get("name", "tool") + " " + json.dumps(x.get("input", {}))[:90]
    return "(working…)"

def first_user(head):
    for line in head.splitlines():
        try:
            d = json.loads(line)
        except Exception:
            continue
        if d.get("type") == "user":
            c = d.get("message", {}).get("content")
            s = c if isinstance(c, str) else " ".join(x.get("text", "") for x in c if isinstance(x, dict)) if isinstance(c, list) else ""
            s = s.strip()
            if s: return s[:120]
    return ""

def session_summary(path, name):
    try:
        head, tail, sz = head_tail(path)
    except Exception as e:
        return None
    t0 = next((parse_dt(ts_of(l)) for l in head.splitlines() if ts_of(l)), None)
    t1 = None
    for l in reversed(tail.splitlines()):
        t = ts_of(l)
        if t: t1 = parse_dt(t); break
    now = time.time()
    wtm = re.search(r"(?:worktrees/agent-|worktree-agent-)([0-9a-f]{6,})", head + tail)
    return {
        "name": name, "label": first_user(head), "last": last_text(tail),
        "size_kb": round(sz / 1024), "dur_s": int((t1 - t0)) if (t0 and t1) else None,
        "age_s": int(now - os.path.getmtime(path)), "wt": wtm.group(1) if wtm else None,
    }

def _tid_of_label(lbl):
    m = re.search(r"(?:task|fixing task|implementing task)\s+(T-\d+)", lbl, re.IGNORECASE)
    if m:
        return m.group(1).upper()
    m2 = re.search(r"\bT-\d+\b", lbl, re.IGNORECASE)
    return m2.group(0).upper() if m2 else ""

def ticket_transcript(tid):
    """The agent's narration for a ticket as markdown: assistant text + tool-use
    one-liners from the best-matching subagent transcript (largest, then newest)."""
    tid = (tid or "").upper()
    best = None  # (size, mtime, path)
    for p in glob.glob(f"{PROJ}/{COORD}/**/*.jsonl", recursive=True):
        try:
            head, _, sz = head_tail(p)
        except Exception:
            continue
        if _tid_of_label(first_user(head)) != tid:
            continue
        mt = os.path.getmtime(p)
        if best is None or (sz, mt) > (best[0], best[1]):
            best = (sz, mt, p)
    if not best:
        return None
    PER = 20000          # per tool-call input / tool-result char cap
    TOTAL = 4_000_000    # overall cap (a safety valve for the browser)
    parts, total, truncated = [], [0], [False]
    def push(s):
        if truncated[0]:
            return
        if total[0] + len(s) + 2 > TOTAL:
            parts.append("\n\n*…(transcript truncated at %d MB — ask to raise the cap for more)…*" % (TOTAL // 1_000_000))
            truncated[0] = True
            return
        parts.append(s)
        total[0] += len(s) + 2
    def clip(s):
        s = s if isinstance(s, str) else json.dumps(s, ensure_ascii=False)
        return s if len(s) <= PER else s[:PER] + "\n…(item truncated at %d KB)…" % (PER // 1000)
    def fmt_tool(name, inp):
        g = (lambda k: inp.get(k)) if isinstance(inp, dict) else (lambda k: None)
        H = "**▷ " + name + "**"
        if name == "Bash":
            desc = g("description")
            return H + (" — " + str(desc) if desc else "") + "\n```bash\n" + clip(str(g("command") or "")) + "\n```"
        if name == "Read":
            extra = f" (offset {g('offset')}, limit {g('limit')})" if (g("offset") or g("limit")) else ""
            return "**▷ Read** `" + str(g("file_path") or "") + "`" + extra
        if name == "Edit":
            old = "\n".join("- " + l for l in clip(str(g("old_string") or "")).split("\n"))
            new = "\n".join("+ " + l for l in clip(str(g("new_string") or "")).split("\n"))
            return "**▷ Edit** `" + str(g("file_path") or "") + "`\n```diff\n" + old + "\n" + new + "\n```"
        if name == "Write":
            return "**▷ Write** `" + str(g("file_path") or "") + "`\n```\n" + clip(str(g("content") or "")) + "\n```"
        if name == "Grep":
            where = g("path") or g("glob")
            return "**▷ Grep** `" + str(g("pattern") or "") + "`" + (" in `" + str(where) + "`" if where else "")
        if name == "Glob":
            return "**▷ Glob** `" + str(g("pattern") or "") + "`" + (" in `" + str(g("path")) + "`" if g("path") else "")
        if name in ("Task", "Agent"):
            return "**▷ " + name + "** — " + str(g("description") or "") + "\n```\n" + clip(str(g("prompt") or "")) + "\n```"
        if name == "TodoWrite":
            todos = g("todos") or []
            return "**▷ TodoWrite**\n" + "\n".join("- [" + str(td.get("status", "")) + "] " + str(td.get("content", "")) for td in todos if isinstance(td, dict))
        return H + "\n```json\n" + clip(json.dumps(inp, indent=1, ensure_ascii=False)) + "\n```"
    try:
        with open(best[2], "r", errors="replace") as f:
            for line in f:
                if truncated[0]:
                    break
                try:
                    d = json.loads(line)
                except Exception:
                    continue
                typ = d.get("type")
                if typ not in ("assistant", "user"):
                    continue
                c = d.get("message", {}).get("content")
                if isinstance(c, str):
                    if typ == "user" and c.strip():
                        push("**\U0001f464 user**\n\n" + clip(c.strip()))
                    continue
                if not isinstance(c, list):
                    continue
                for x in c:
                    if not isinstance(x, dict):
                        continue
                    t = x.get("type")
                    if t == "text" and x.get("text", "").strip():
                        push(x["text"].strip())
                    elif t == "tool_use":
                        push(fmt_tool(str(x.get("name", "tool")), x.get("input", {})))
                    elif t == "tool_result":
                        rc = x.get("content")
                        if isinstance(rc, list):
                            rc = "\n".join(y.get("text", "") for y in rc if isinstance(y, dict) and y.get("type") == "text") or json.dumps(rc, ensure_ascii=False)
                        rc = (rc if isinstance(rc, str) else json.dumps(rc, ensure_ascii=False)).strip()
                        if rc:
                            push("**◀ result**\n```\n" + clip(rc) + "\n```")
    except Exception as e:
        return {"id": tid, "markdown": f"(error reading transcript: {e})"}
    md = "\n\n".join(parts).strip() or "(no content in this transcript)"
    return {"id": tid, "markdown": md, "file": os.path.basename(best[2]), "size_kb": round(best[0] / 1024)}

_TASKS_YAML_CACHE = {"mtime": None, "data": None}

def load_tasks_yaml():
    """Cached parse of docs/tasks.yaml (list of task dicts), keyed by file mtime."""
    path = f"{REPO}/docs/tasks.yaml"
    try:
        mtime = os.path.getmtime(path)
    except Exception:
        return []
    c = _TASKS_YAML_CACHE
    if c["data"] is not None and c["mtime"] == mtime:
        return c["data"]
    try:
        import yaml
        d = yaml.safe_load(open(path))
        tl = d["tasks"] if isinstance(d, dict) else d
    except Exception:
        tl = []
    c.update(mtime=mtime, data=tl)
    return tl

_TICKET_ID_RE = re.compile(r"^T-\d+$")
_TICKET_DETAIL_FIELDS = ("id", "title", "status", "milestone", "priority", "model", "effort",
                         "parallel_group", "area", "commit", "acceptance", "use_cases",
                         "found_by", "notes")

def git_timing():
    """Per-ticket first/last commit time from git log (T-### mentions). Rebuilt after
    the crash-recovery reconstruction; lighter than the original (no cache)."""
    try:
        out = sh(["git", "log", "--no-merges", "--pretty=%cI\x1f%s"], cwd=REPO, timeout=15)
    except Exception:
        return {"tickets": {}}
    tickets = {}
    for line in out.splitlines():
        parts = line.split("\x1f", 1)
        if len(parts) != 2:
            continue
        ts, subj = parts
        for m in re.finditer(r"T-0*(\d+)", subj, re.IGNORECASE):
            tid = "T-" + m.group(1)
            t = tickets.setdefault(tid, {"start": ts, "end": ts, "commits": 0})
            t["commits"] += 1
            if ts < t["start"]: t["start"] = ts
            if ts > t["end"]: t["end"] = ts
    return {"tickets": tickets}

def ticket_detail(tid):
    """Full record for one ticket id, for the click-through modal: raw tasks.yaml
    fields plus git-derived timing. {} for unknown/non-ticket ids."""
    tid = (tid or "").strip().upper()
    if not _TICKET_ID_RE.match(tid):
        return {}
    x = next((t for t in load_tasks_yaml() if t.get("id") == tid), None)
    if x is None:
        return {}
    out = {k: x.get(k) for k in _TICKET_DETAIL_FIELDS if x.get(k) not in (None, "", [])}
    out["id"] = tid
    out["deps"] = x.get("deps") or x.get("depends_on") or []
    out["blocked_on"] = x.get("blocked_on") or []
    try:
        out["timing"] = git_timing()["tickets"].get(tid)
    except Exception:
        out["timing"] = None
    return out

def agents(status_map):
    out = []
    titles = {t.get("id"): t.get("title", "") for t in load_tasks_yaml()}
    coord_path = f"{PROJ}/{COORD}.jsonl"
    if os.path.exists(coord_path):
        s = session_summary(coord_path, "coordinator")
        if s: s["status"] = None; s["running"] = s["age_s"] < 180; out.append(s)
    subs = glob.glob(f"{PROJ}/{COORD}/**/*.jsonl", recursive=True)
    subs = [p for p in subs if os.path.getmtime(p) > time.time() - 1800]
    subs.sort(key=os.path.getmtime, reverse=True)
    ACTIVE = 210   # a subagent quiet longer than this is treated as no longer running
    best = {}       # dedupe by task id, keep the freshest transcript
    for p in subs:
        s = session_summary(p, "agent")
        if not s: continue
        m = re.search(r"(?:task|fixing task|implementing task)\s+(T-\d+)", s["label"], re.IGNORECASE)
        if m:
            tid = m.group(1).upper()
        else:
            # Fallback: the launch prompt's wording changed, but the ticket id is
            # still in it somewhere — take the first T-### we see.
            m2 = re.search(r"\bT-\d+\b", s["label"], re.IGNORECASE)
            tid = m2.group(0).upper() if m2 else None
        st = status_map.get(tid)
        if st in ("done", "cancelled"): continue     # merged already
        if s["age_s"] > ACTIVE: continue              # gone quiet: not actually running
        key = tid or p
        if key not in best or s["age_s"] < best[key]["age_s"]:
            s["name"] = tid or "agent"; s["status"] = st; s["running"] = True; s["title"] = titles.get(tid, "")
            best[key] = s
    out += sorted(best.values(), key=lambda a: (ticket_num(a["name"]), a["name"]))
    return out

import threading, collections
try:
    import psutil
    psutil.cpu_percent(percpu=True)  # prime the counter
except Exception:
    psutil = None

# Background sampler: per-core CPU every 1 s, keep the last 5 s for a smooth average.
CPU_HIST = collections.deque(maxlen=5)
def _cpu_sampler():
    while psutil is not None:
        try:
            CPU_HIST.append(psutil.cpu_percent(percpu=True, interval=1.0))
        except Exception:
            time.sleep(1)

def system_load():
    try:
        l1, l5, l15 = os.getloadavg()
    except Exception:
        l1 = l5 = l15 = 0.0
    cores = os.cpu_count() or 1
    def count(pat):
        try:
            out = subprocess.run(["pgrep", "-f", pat], capture_output=True, text=True, timeout=4).stdout
            return len([x for x in out.split() if x])
        except Exception:
            return 0
    return {"load1": round(l1, 1), "load5": round(l5, 1), "cores": cores,
            "rustc": count(r"bin/rustc"), "cargo": count(r"cargo (build|nextest|test)")}

def sysstats():
    if psutil is None:
        return {"cores": os.cpu_count() or 1, "per": [], "mem": {}}
    series = [[round(x) for x in snap] for snap in CPU_HIST]   # up to 5 one-second samples, oldest→newest
    per = series[-1] if series else [round(x) for x in psutil.cpu_percent(percpu=True)]
    vm = psutil.virtual_memory()
    sat = sum(1 for x in per if x >= 80)
    try:
        du = psutil.disk_usage("/System/Volumes/Data")
        disk = {"pct": round(du.percent), "free_gb": round(du.free / 1e9), "total_gb": round(du.total / 1e9)}
    except Exception:
        disk = {}
    return {
        "cores": len(per), "per": per, "series": series, "saturated": sat,
        "busy": round(sum(per) / 100, 1),   # core-equivalents of work
        "mem": {"pct": round(vm.percent), "used_gb": round(vm.used / 1e9, 1), "total_gb": round(vm.total / 1e9)},
        "disk": disk,
    }

def gather():
    tk = tasks()
    smap = tk.get("status_map", {})
    ags = agents(smap)
    active_wts = {a["wt"] for a in ags if a.get("wt") and a["name"] != "coordinator"}
    task_by_hash = {a["wt"]: a["name"] for a in ags if a.get("wt") and a.get("name", "").startswith("T-")}
    wt = worktrees(active_wts)
    for w in wt:
        if not w["is_main"]:
            for h, t in task_by_hash.items():
                if h in (w["path"] + w["branch"]):
                    w["task"] = t; break
    wt.sort(key=lambda w: (not w["is_main"], ticket_num(w.get("task")), w["name"]))
    mg = merge_status()
    return {
        "now": time.strftime("%Y-%m-%d %H:%M:%S %Z"),
        "worktrees": wt, "tasks": tk, "log": git_log(),
        "coord": coord_pane(), "agents": ags, "sys": system_load(), "stage": stage_status(),
        "merge": mg, "queue": work_queue(smap, wt, ags, mg.get("ticket", "")),
        "budget": budget_status(),
    }

PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>hackriff agents</title>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--teal:#52C2AE;--amber:#F0A542;--lav:#A395E0;--coral:#E47B68;--mono:"SFMono-Regular",Menlo,monospace}
*{box-sizing:border-box}html,body{height:100%;max-width:100%}body{margin:0;background:var(--bg);color:var(--txt);font:13px/1.5 -apple-system,system-ui,sans-serif;overflow:hidden;overflow-x:hidden}
.ag .lbl,.ag .last,.wt b,.wt .br,.wt .ds,.wt .cm{overflow-wrap:anywhere;word-break:break-word}
.app{height:100vh;display:flex;flex-direction:column;overflow:hidden}
h1{font-size:15px;margin:0;letter-spacing:.02em;white-space:nowrap}h1 b{color:var(--amber)}
.top{display:flex;align-items:center;gap:12px;flex-wrap:wrap;padding:8px 14px;border-bottom:1px solid var(--line);background:var(--panel)}
.top .t{color:var(--dim);font:11.5px var(--mono);white-space:nowrap}
.pill{font:11px var(--mono);padding:2px 8px;border:1px solid var(--line);border-radius:20px;color:var(--mut);white-space:nowrap}
.dot{display:inline-block;width:7px;height:7px;border-radius:50%;background:var(--teal);margin-right:5px}
.top .counts{margin-left:auto}
.cols{flex:1;min-height:0;display:grid;grid-template-columns:1.5fr 1fr 1.15fr;gap:10px;padding:10px}
.col{display:flex;flex-direction:column;gap:10px;min-height:0;min-width:0}
.card{background:var(--panel);border:1px solid var(--line);border-radius:9px;padding:10px 12px;display:flex;flex-direction:column;min-height:0;min-width:0}
.card.fill{flex:1}
.card h2{font-size:11px;text-transform:uppercase;letter-spacing:.09em;color:var(--mut);margin:0 0 8px;display:flex;justify-content:space-between;flex:0 0 auto}
.card h2 em{font-style:normal;color:var(--dim)}
.bd{overflow:auto;min-height:0;flex:1}
.counts{display:flex;gap:6px;flex-wrap:wrap}
.chip{font:11px var(--mono);padding:2px 8px;border-radius:5px;border:1px solid var(--line);color:var(--mut)}
.chip.done{color:var(--teal)}.chip.in-progress{color:var(--amber)}.chip.blocked,.chip.paused{color:var(--coral)}.chip.todo{color:var(--lav)}
.chip.ms{color:var(--dim);border-color:var(--line);letter-spacing:.04em}
.qsec{font:10px var(--mono);letter-spacing:.08em;text-transform:uppercase;color:var(--dim);margin:10px 0 5px;display:flex;justify-content:space-between}
.qsec:first-child{margin-top:0}
.qsec .hint{text-transform:none;letter-spacing:0;color:var(--dim);font-weight:400}
.qrow{display:flex;align-items:baseline;gap:7px;padding:3px 0;border-top:1px solid var(--line)}
.qrow:first-of-type{border-top:0}
.qrow .qt{color:var(--mut);flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.qrow.merging{background:rgba(163,149,224,.10)}
.qi{font:10px var(--mono);padding:1px 6px;border-radius:4px;border:1px solid var(--line);color:var(--mut);white-space:nowrap}
.qi.merging{color:var(--lav);border-color:rgba(163,149,224,.5)}
.qi.ready{color:var(--teal);border-color:rgba(82,194,174,.4)}
.qi.building{color:var(--amber);border-color:rgba(240,165,66,.4)}
.qi.wait{color:var(--dim)}
.qi.pri-high{color:var(--coral);border-color:rgba(228,123,104,.4)}
.qi.pri-medium{color:var(--amber)}.qi.pri-low{color:var(--dim)}
.qi.user{color:var(--lav);border-color:rgba(163,149,224,.4)}
.qnum{color:var(--dim);font:10px var(--mono);width:16px;text-align:right}
.wt{border-top:1px solid var(--line);padding:9px 0}
.wt:first-of-type{border-top:0}
.wt .r1{display:flex;justify-content:space-between;gap:8px;align-items:baseline}
.wt b{font:12px var(--mono)}.wt .br{color:var(--lav);font:11px var(--mono)}
.wt-task{color:var(--amber);font-weight:600}.wt-hash{color:var(--dim);font-size:10.5px;font-weight:400}
.wt .ds{color:var(--teal);font:11px var(--mono);margin-top:2px}
.wt .ds-note{color:var(--dim);font:10px var(--mono)}
.wt .files{color:var(--dim);font:11px var(--mono);margin-top:3px;max-height:120px;overflow:auto;white-space:pre-wrap}
.wt .cm{color:var(--mut);font-size:11.5px;margin-top:3px}
.wt.clean b{color:var(--dim)}
.badge{font:10px var(--mono);padding:1px 6px;border-radius:4px}
.badge.chg{color:var(--amber);border:1px solid rgba(240,165,66,.4)}
.badge.clean{color:var(--dim);border:1px solid var(--line)}
.ag{border-top:1px solid var(--line);padding:9px 0;height:120px;display:flex;flex-direction:column;gap:3px}.ag:first-of-type{border-top:0}
.ag .r1{display:flex;justify-content:space-between;gap:8px;align-items:baseline;flex:0 0 auto}
.ag .nm{font:12px var(--mono);color:var(--teal);display:inline-flex;align-items:center;gap:6px}.ag .nm.coordinator{color:var(--amber)}
.rdot{width:7px;height:7px;border-radius:50%;display:inline-block}.rdot.on{background:var(--teal);box-shadow:0 0 0 2px rgba(82,194,174,.2)}.rdot.off{background:var(--dim)}
.ag .meta{color:var(--dim);font:11px var(--mono)}
.ag .nm{min-width:0}
.ag .agtitle{color:var(--mut);font-size:11.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0}
.ag .lbl{color:var(--mut);font-size:11.5px;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.ag .last{color:var(--txt);font-size:12px;background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:6px 8px;flex:1;min-height:0;overflow:auto;white-space:pre-wrap;word-break:break-word}
pre.pane{margin:0;font:11.5px/1.5 var(--mono);color:var(--mut);white-space:pre-wrap;word-break:break-word;background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:8px 10px;overflow:auto;flex:1;min-height:0}
.log{font:11.5px var(--mono)}.log div{padding:2px 0;color:var(--mut);border-top:1px solid var(--line);overflow-wrap:anywhere;word-break:break-word}
.log div:first-child{border-top:0}.log .h{color:var(--teal)}.log .w{color:var(--dim)}
.err{color:var(--coral)}
*{scrollbar-width:thin;scrollbar-color:transparent transparent}
::-webkit-scrollbar{width:8px;height:8px}
::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:transparent;border-radius:4px}
:hover::-webkit-scrollbar-thumb{background:rgba(133,149,160,.4)}
::-webkit-scrollbar-thumb:hover{background:rgba(133,149,160,.7)}
:hover{scrollbar-color:rgba(133,149,160,.4) transparent}
.maplink{color:var(--amber);text-decoration:none;border:1px solid rgba(240,165,66,.4);border-radius:6px;padding:2px 9px;font-size:11.5px}
.maplink:hover{background:rgba(240,165,66,.12)}
.cpu-wrap{background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:8px 8px 6px}
.cores{display:flex;align-items:flex-end;gap:2px;height:46px}
.core{flex:1;min-width:0;height:100%;display:flex;flex-direction:column;justify-content:flex-end;border-radius:2px;overflow:hidden;background:#0a0f12;position:relative}
.core i{display:block;width:100%;height:100%;transform:scaleY(0);transform-origin:bottom;transition:transform 1s linear,background 1s linear;will-change:transform;backface-visibility:hidden}
@media(prefers-reduced-motion:reduce){.core i{transition:none}}
.gauges{margin-top:10px;display:grid;grid-template-columns:1fr 1fr;gap:10px}
.mem-lbl{display:flex;justify-content:space-between;gap:6px;font:11px var(--mono);color:var(--mut);margin-bottom:3px;white-space:nowrap}
.mem-bar{height:12px;border-radius:6px;background:#0a0f12;border:1px solid var(--line);overflow:hidden}
.mem-bar i{display:block;height:100%;width:0;background:linear-gradient(90deg,#3a6ea5,#52C2AE);transition:width .4s ease}
@media(max-width:1000px){.cols{grid-template-columns:1fr 1fr}}
@media(max-width:640px){
  body{overflow:auto;overflow-x:hidden;font-size:12px}
  .app{height:auto;overflow:visible}
  .top{padding:8px 12px;gap:8px 10px}
  h1{font-size:14px}
  .top .t,.pill{font-size:10.5px}
  .cols{grid-template-columns:1fr;height:auto;gap:8px;padding:8px}
  .card{padding:9px 11px}
  .card.fill{flex:none}
  .bd{max-height:46vh}
  pre.pane{font-size:11px;max-height:38vh}
  .card [style*="max-height"]{max-height:none!important}
  .cores{height:40px;gap:1px}
  h2{font-size:10.5px}
  .col{display:contents}            /* promote cards to direct items so order works */
  #syscard{order:-1}                /* System stats first on mobile */
}
</style></head><body><div class=app>
<div class=top><h1>hack<b>riff</b> · agents</h1><span class=pill><span class=dot></span><span id=st>live</span></span><span class=t id=now></span><span class=pill id=load></span><span class=pill id=merge title="Is the coordinator handling the merge queue?"></span><span class=pill id=budget title="Claude token budget. Fed from /usage; update: curl 'http://127.0.0.1:8901/budget?weekly=90&session=3'"></span><a class=maplink href="/terminal">terminal ↗</a><a class=maplink href="/graph">task map ↗</a><span class=t id=err></span><span class=counts id=counts></span></div>
<div class=cols>
  <div class=col>
    <div class="card fill"><h2>Agents <em id=agn></em></h2><div class=bd id=agents></div></div>
  </div>
  <div class=col>
    <div class="card" id=queuecard style="flex:0 0 auto;max-height:56%"><h2>Queue <em id=qn></em></h2><div class=bd id=queue></div></div>
    <div class="card fill"><h2>Work trees <em id=wtn></em></h2><div class=bd id=wts></div></div>
  </div>
  <div class=col>
    <div class="card" id=syscard style="flex:0 0 auto"><h2>System <em id=sys-sub></em></h2>
      <div class="cpu-wrap"><div class="cores" id=cores></div></div>
      <div class="gauges">
        <div><div class="mem-lbl"><span>Memory</span><span id=mem-txt></span></div><div class="mem-bar"><i id=mem-fill></i></div></div>
        <div><div class="mem-lbl"><span>Disk free</span><span id=disk-txt></span></div><div class="mem-bar"><i id=disk-fill></i></div></div>
      </div>
    </div>
    <div class="card" style="flex:0 0 auto;max-height:52%"><h2>Tasks <em id=tkn></em></h2><div class="bd log" id=active></div></div>
    <div class="card fill"><h2>Recent commits <em>main</em></h2><div class="bd log" id=log></div></div>
    <div class="card" style="flex:0 0 auto;height:190px"><h2>Staging server <em id=stage-sub></em></h2><div class="bd log" id=stage></div></div>
  </div>
</div></div>
<script>
const $=s=>document.querySelector(s), esc=s=>(s||"").replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));
const dur=s=>s==null?"—":s<60?s+"s":s<3600?Math.floor(s/60)+"m "+(s%60)+"s":Math.floor(s/3600)+"h "+Math.floor(s%3600/60)+"m";
async function tick(){
 try{
  const d=await (await fetch('/data.json',{cache:'no-store'})).json();
  $('#now').textContent=d.now; $('#err').textContent=''; $('#st').textContent='live';
  const pn=document.getElementById('pane'); if(pn) pn.textContent=d.coord||'(no pane)';
  const y=d.sys||{}; const busy=(y.load1||0)>(y.cores||28)*0.9;
  const el=$('#load'); el.textContent=`load ${y.load1}/${y.cores} · ${y.rustc} rustc · ${y.cargo} cargo`;
  el.style.color=busy?'#E47B68':(((y.load1||0)>(y.cores||28)*0.6)?'#F0A542':'#52C2AE');
  const mg=d.merge||{state:'idle'};
  const mgEl=$('#merge');
  if(mg.state==='merging'){ mgEl.textContent='⇄ merging'+(mg.gate?' · gate '+dur(mg.elapsed_s):''); mgEl.style.color='#A395E0'; mgEl.title='Merging: '+(mg.msg||'?'); }
  else if(mg.state==='gating'){ mgEl.textContent='⚙ '+mg.gate+' · '+dur(mg.elapsed_s); mgEl.style.color='#F0A542'; mgEl.title='Gate running before merge'; }
  else { mgEl.textContent='idle'+(mg.queue?' · '+mg.queue+' in-progress':''); mgEl.style.color='#5A6973'; mgEl.title='No merge or gate running'; }
  const b=d.budget||{}; const bEl=$('#budget');
  if(b.weekly!=null||b.session!=null){
    const stale=b.age_s!=null&&b.age_s>3*3600;
    const wk=b.weekly!=null?b.weekly:'?', se=b.session!=null?b.session:'?';
    bEl.textContent=`budget ${wk}% wk · ${se}% sess`+(stale?` · ${dur(b.age_s)} old`:'');
    bEl.style.color = (b.weekly>=90)?'#E47B68':(b.weekly>=75)?'#F0A542':(stale?'#5A6973':'#52C2AE');
    const src=b.auto?'auto-polled from /usage':'manually fed';
    const rs=[b.weekly_reset?'weekly resets '+b.weekly_reset:'',b.session_reset?'session resets '+b.session_reset:''].filter(Boolean).join(' · ');
    bEl.title=`Claude token budget — ${src}, ${b.age_s!=null?dur(b.age_s)+' ago':'never'}. ${rs}. Force refresh: /pollbudget`;
    bEl.hidden=false;
  } else { bEl.hidden=true; }
  const A=d.agents||[]; $('#agn').textContent=A.length+' running';
  const idleWhy = mg.state==='merging' ? 'no builder agents — coordinator merging ('+esc(mg.msg||'')+')'
    : mg.state==='gating' ? 'no builder agents — coordinator gating ('+esc(mg.gate)+' '+dur(mg.elapsed_s)+')'
    : 'no agents running — coordinator idle';
  $('#agents').innerHTML=A.map(a=>`<div class=ag><div class=r1><span class="nm ${a.name==='coordinator'?'coordinator':''}"><span class="rdot ${a.running?'on':'off'}"></span>${/^T-\d/.test(a.name)?`<span class=tlink data-tid="${esc(a.name)}">${esc(a.name)}</span>`:esc(a.name)}${a.status?` <span class="chip ${a.status}">${a.status}</span>`:''}${a.title?` <span class=agtitle>${esc(a.title)}</span>`:''}</span><span class=meta>${a.name==='coordinator'?'':dur(a.dur_s)+' · '}last ${dur(a.age_s)} ago</span></div>${a.label?`<div class=lbl>${esc(a.label)}</div>`:''}<div class=last>${esc(a.last)}</div></div>`).join('')||`<div class=lbl>${idleWhy}</div>`;
  const W=d.worktrees||[]; $('#wtn').textContent=W.length+' trees';
  $('#wts').innerHTML=W.map(w=>{
    const badge = w.is_main ? (w.uncommitted?`<span class="badge chg">${w.uncommitted} uncommitted</span>`:`<span class="badge clean">clean</span>`)
      : `<span class="badge ${w.changed?'chg':'clean'}">${w.ahead||0} commit${w.ahead===1?'':'s'} vs main${w.uncommitted?` · ${w.uncommitted} wip`:''}</span>`;
    const scope = w.is_main ? '' : `<span class=ds-note>vs fork point on main</span>`;
    const title = w.is_main ? 'main' : (w.task ? `<span class=wt-task><span class=tlink data-tid="${esc(w.task)}">${esc(w.task)}</span></span> <span class=wt-hash>${esc(w.name)}</span>` : `<span class=wt-hash>${esc(w.name)}</span>`);
    return `<div class="wt ${w.changed?'':'clean'}"><div class=r1><b>${title}</b>${badge}</div><div class=br>${esc(w.branch)}</div>${w.diffstat?`<div class=ds>${esc(w.diffstat)} ${scope}</div>`:''}${w.files.length?`<div class=files>${w.files.map(esc).join('\n')}</div>`:''}<div class=cm>${esc(w.commit_when)} · ${esc(w.commit_msg)}</div></div>`;
  }).join('');
  // --- Queue: merge queue (branches waiting to land) + derived upcoming order ---
  const q=d.queue||{merge_ready:[],upcoming:[]};
  const tk_=id=>`<span class=tlink data-tid="${esc(id)}">${esc(id)}</span>`;
  const mr=q.merge_ready||[];
  let qh='';
  qh+=`<div class=qsec><span>Merge queue</span><span class=hint>${mr.length} branch${mr.length===1?'':'es'} · serial gate</span></div>`;
  if(mr.length){
    mr.forEach(r=>{
      const tag = r.merging?`<span class="qi merging">⇄ merging</span>`
        : r.ahead>0?`<span class="qi ready">${r.ahead} commit${r.ahead===1?'':'s'} · queued</span>`
        : r.building?`<span class="qi building">building</span>`
        : `<span class="qi wait" title="in-progress but 0 commits vs main — likely merged already (board lag) or not started; reconcile will resolve">0 vs main — check board</span>`;
      qh+=`<div class="qrow ${r.merging?'merging':''}"><b>${tk_(r.id)}</b>${tag}<span class=qt title="${esc(r.title)}">${esc(r.title)}</span></div>`;
    });
  } else qh+=`<div class=qrow><span class=qt>nothing waiting to merge</span></div>`;
  const up=q.upcoming||[];
  const amsTxt=(q.active_ms||[]).join(', ');
  qh+=`<div class=qsec><span>Up next</span><span class=hint title="Derived order: user-requested, then tickets in the milestone(s) the coordinator is working now (${esc(amsTxt||'—')}), then priority, then number. ~45% top-3 accurate vs history — a guide to what's coming, not an exact schedule.">derived · ${q.todo_total||0} todo</span></div>`;
  up.forEach((r,i)=>{
    const chips=[];
    if(r.user) chips.push(`<span class="qi user">user</span>`);
    if(r.active_ms&&r.milestone) chips.push(`<span class="qi ready" title="milestone in progress now">${esc(r.milestone)}</span>`);
    else if(r.milestone) chips.push(`<span class="qi" style="color:var(--dim)">${esc(r.milestone)}</span>`);
    if(r.priority&&r.priority!=='normal') chips.push(`<span class="qi pri-${r.priority}">${r.priority}</span>`);
    if(!r.ready) chips.push(`<span class="qi wait" title="waiting on ${esc((r.waiting_on||[]).join(', '))}">deps: ${esc((r.waiting_on||[]).join(','))}</span>`);
    qh+=`<div class=qrow><span class=qnum>${i+1}</span><b>${tk_(r.id)}</b>${chips.join('')}<span class=qt title="${esc(r.title)}">${esc(r.title)}</span></div>`;
  });
  $('#queue').innerHTML=qh;
  $('#qn').textContent=`${mr.length} merging/queued · ${q.todo_total||0} todo`;
  const t=d.tasks||{counts:{},active:[]}; $('#tkn').textContent=t.total+' total';
  $('#counts').innerHTML=Object.entries(t.counts).sort().map(([k,v])=>`<span class="chip ${k}">${k} ${v}</span>`).join('');
  $('#active').innerHTML=(t.active||[]).map(x=>`<div><span class="chip ${x.status}">${x.status}</span>${x.milestone?`<span class="chip ms">${esc(x.milestone)}</span>`:''} <b><span class=tlink data-tid="${esc(x.id)}">${esc(x.id)}</span></b> ${esc(x.title)}</div>`).join('')||'<div class=w>none in progress</div>';
  $('#log').innerHTML=(d.log||[]).map(r=>`<div><span class=h>${r.h}</span> <span class=w>${esc(r.when)}</span><br>${esc(r.msg)}</div>`).join('');
  const s=d.stage||{}; const sm=s.smoke==='ok'?'#52C2AE':s.smoke==='fail'?'#E47B68':'#8595A0';
  $('#stage-sub').innerHTML=`<span class="rdot ${s.alive?'on':'off'}"></span>${s.alive?'watching':'stopped'} · ${esc(s.source||'?')} · ${esc(s.built||'')} · <span style="color:${sm}">smoke ${s.smoke||'—'}</span>`;
  const stEl=$('#stage'); const atBottom=stEl.scrollHeight-stEl.scrollTop-stEl.clientHeight<20;
  stEl.innerHTML=(s.lines||[]).map(l=>{const c=l.includes('FAIL')?'#E47B68':l.includes('SMOKE OK')||l.includes('started')?'#52C2AE':l.includes('new code')?'#F0A542':'var(--mut)';return `<div style="color:${c}">${esc(l)}</div>`;}).join('')||'<div class=w>no staging log yet</div>';
  if(atBottom) stEl.scrollTop=stEl.scrollHeight;
 }catch(e){ $('#err').textContent='fetch error: '+e; $('#st').textContent='retrying'; }
}
tick(); setInterval(tick,5000);
const coreEl=$('#cores'); let cells=[];
function coreColor(v){ return v>=85?'#E47B68':v>=55?'#F0A542':v>=20?'#52C2AE':'#2E4A5B'; }
function buildCores(n){ coreEl.innerHTML=''; cells=[]; for(let i=0;i<n;i++){ const c=document.createElement('div'); c.className='core'; const f=document.createElement('i'); c.appendChild(f); c.title='core '+i; coreEl.appendChild(c); cells.push(f);} }
let coreFrames=[], frameIdx=0;
function playFrame(){
  if(!coreFrames.length) return;
  const f=coreFrames[Math.min(frameIdx,coreFrames.length-1)];
  if(cells.length!==f.length) buildCores(f.length);
  f.forEach((v,i)=>{ const c=cells[i]; if(!c) return; c.style.transform='scaleY('+Math.max(0.03,v/100).toFixed(4)+')'; c.style.background=coreColor(v); });
  frameIdx++;
}
async function sysTick(){
 try{
  const s=await (await fetch('/sys.json',{cache:'no-store'})).json();
  coreFrames=(s.series&&s.series.length)?s.series:[s.per||[]]; frameIdx=0; playFrame();
  $('#sys-sub').textContent=`${s.saturated||0}/${s.cores} cores saturated · ${s.busy||0} busy`;
  const m=s.mem||{}; $('#mem-fill').style.width=(m.pct||0)+'%'; $('#mem-txt').textContent=`${m.used_gb||0} / ${m.total_gb||0} GB · ${m.pct||0}%`;
  $('#mem-fill').style.background=(m.pct||0)>=85?'linear-gradient(90deg,#F0A542,#E47B68)':'linear-gradient(90deg,#3a6ea5,#52C2AE)';
  const dk=s.disk||{}; const free=dk.free_gb; if(free!=null){
    $('#disk-txt').textContent=`${free} GB · ${dk.pct}% used`;
    $('#disk-fill').style.width=(dk.pct||0)+'%';
    const col=free<10?'linear-gradient(90deg,#E47B68,#E47B68)':free<25?'linear-gradient(90deg,#F0A542,#E47B68)':'linear-gradient(90deg,#3a6ea5,#52C2AE)';
    $('#disk-fill').style.background=col;
    $('#disk-txt').style.color=free<10?'#E47B68':free<25?'#F0A542':'var(--mut)';
  }
 }catch(e){}
}
sysTick(); setInterval(sysTick,5000); setInterval(playFrame,1000);
</script></body></html>"""

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path.startswith("/term.json"):
            try:
                body = json.dumps({"coord": term_pane(), "now": time.strftime("%H:%M:%S %Z")}).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"coord": str(e)}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/terminal"):
            body = TERM_PAGE.encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/graph.json"):
            scope = "all" if "scope=all" in self.path else "frontier"
            show_done = "done=0" not in self.path
            show_todo = "todo=0" not in self.path
            show_blocked = "blocked=0" not in self.path
            try:
                body = json.dumps(task_graph(scope, show_done, show_todo, show_blocked)).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": str(e), "mermaid": "graph RL"}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/graph"):
            body = GRAPH_PAGE.replace("</body>", TICKET_MODAL + "</body>").encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/sys.json"):
            try:
                body = json.dumps(sysstats()).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": str(e)}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/pollbudget"):
            u = poll_usage()
            body = json.dumps(u or {"error": "no data"}).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/budget"):
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            try:
                cur = json.load(open(USAGE_FILE))
            except Exception:
                cur = {}
            for k in ("weekly", "session"):
                if k in q:
                    try:
                        cur[k] = round(float(q[k][0]), 1)
                    except Exception:
                        pass
            cur["updated"] = time.time()
            try:
                json.dump(cur, open(USAGE_FILE, "w"))
            except Exception:
                pass
            body = json.dumps(cur).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/ticket.json") or self.path.startswith("/transcript.json"):
            m_id = re.search(r"[?&]id=([^&]+)", self.path)
            tid = urllib.parse.unquote(m_id.group(1)) if m_id else ""
            try:
                data = ticket_transcript(tid) if self.path.startswith("/transcript.json") else ticket_detail(tid)
                body = json.dumps(data).encode()
            except Exception as e:
                body = json.dumps({"error": str(e)}).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/timeline.json"):
            # timeline() function was lost in crash-recovery reconstruction; stub to keep the client working.
            body = json.dumps({"rows": [], "note": "timeline data unavailable after crash recovery"}).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/metrics.json"):
            body = json.dumps([]).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/timeline"):
            body = (b"<!doctype html><meta charset=utf-8><title>timeline</title>"
                    b"<body style='font:14px system-ui;background:#0D1317;color:#D5DEE2;padding:2rem'>"
                    b"<h2>Timeline chart</h2><p>This view was lost in the post-crash dashboard reconstruction "
                    b"and is being rebuilt. The main dashboard and task map are unaffected.</p>"
                    b"<p><a style='color:#F0A542' href='/'>&larr; back to dashboard</a></p>")
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Cache-Control", "no-store"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/data.json"):
            try:
                body = json.dumps(gather()).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
            except Exception as e:
                body = json.dumps({"error": str(e)}).encode(); self.send_response(500); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body)
        else:
            body = PAGE.replace("</body>", TICKET_MODAL + "</body>").encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)

if __name__ == "__main__":
    if psutil is not None:
        threading.Thread(target=_cpu_sampler, daemon=True).start()
    threading.Thread(target=_usage_poller, daemon=True).start()
    ThreadingHTTPServer(("127.0.0.1", 8901), H).serve_forever()
