#!/usr/bin/env python3
"""hackriff agent monitor: worktrees, diffs, tasks, coordinator pane, agent sessions."""
import json, os, re, subprocess, glob, time, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPO = "/Users/daniellewis/hackriff"
SCRATCH = os.environ.get("HACKRIFF_OPS", os.path.expanduser("~/.hackriff-ops"))
os.makedirs(SCRATCH, exist_ok=True)
PROJ = "/Users/daniellewis/.claude/projects/-Users-daniellewis-hackriff"
OPSDIR = os.path.dirname(os.path.abspath(__file__))   # so `import perf` (same dir) resolves

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

def _tk_of_branch(b):
    m = re.match(r"^task-t0*(\d+)$", b or "", re.I)
    return f"T-{m.group(1)}" if m else b


def runtime_states(smap, tl=None, limit=10):
    """What the runners know that the board does not: per ticket, one of
    failed (a gate/review/worker failure needing a person), testing (in the current gate),
    queued (waiting for the merge runner), review (in the reviewer stage), next (the runner's
    projected next dispatches). Read from $HACKRIFF_OPS files only; never guessed."""
    st, why = {}, {}
    def put(tid, state, reason=""):
        if tid and smap.get(tid) not in ("done", "cancelled") and tid not in st:
            st[tid] = state; why[tid] = reason
    # The merge runner's view first: a branch in the current gate or the queue file is exactly that,
    # whatever failed earlier (T-800/T-763/T-299 re-queued after load flakes must read QUEUED).
    try:
        bm = dict(l.split("=", 1) for l in open(os.path.join(SCRATCH, "bulk-in-progress")).read().splitlines() if "=" in l)
        for b in bm.get("branches", "").split():
            put(_tk_of_branch(b), "testing", "bulk gate")
    except Exception:
        pass
    try:
        first = open(os.path.join(REPO, ".git", "MERGE_MSG")).read().splitlines()[0]
        m = re.search(r"task-t\d+", first)
        if m:
            put(_tk_of_branch(m.group(0)), "testing", "staged merge")
    except Exception:
        pass
    try:
        for l in open(os.path.join(SCRATCH, "merge-queue.txt")):
            put(_tk_of_branch(l.strip()), "queued", "merge queue")
    except Exception:
        pass
    # Then the work-runner claim: what a ticket IS now outranks what happened to it earlier
    # (a branch that failed a gate at 09:22 and is queued again at 12:00 is QUEUED, not FAILED - the
    # old precedence made 34 "failed" out of 3 real ones on 2026-09-22). NO_WORK is a stopped or lost
    # agent, drawn as "stopped", never as a failure.
    try:
        _claims = json.load(open(os.path.join(SCRATCH, "work-claims.json")))
        for tid, c in _claims.items():
            if c.get("deflake"):
                continue   # a DEFLAKE:<slug> claim (ops/work-runner.py dispatch_deflakes) is not a board ticket
            cs, ck = c.get("state"), c.get("kind")
            if cs == "running" and ck == "review":
                put(tid, "review", "reviewer stage")
            elif cs == "running" and ck == "fix":
                put(tid, "failed", "fixing a gate/review failure")
            elif cs == "running":
                pass   # working: drawn by the agents() path, not here
            elif cs == "queued":
                put(tid, "queued", "merge queue")
            elif cs == "blocked":
                put(tid, "blocked", "worker handed back BLOCKED - needs a person")
            elif cs in ("gate-failed", "review-failed", "uncommitted", "cancel-proposed", "error", "timeout"):
                put(tid, "failed", cs.upper().replace("-", "_"))
            elif cs == "no-work":
                put(tid, "stopped", "stopped or lost agent (NO_WORK)")
    except Exception:
        pass
    try:  # failure lines from the attention files: only for tickets with no current claim
        for l in open(os.path.join(SCRATCH, "merge-needs-attention.txt")):
            f = l.split()   # date time branch ticket KIND detail...
            if len(f) >= 5 and f[4].startswith(("GATE_FAIL", "CONFLICT", "GAVE_UP", "UNCHANGED")):
                put(_tk_of_branch(f[2]) if f[3].startswith("task-") else f[3], "failed", f[4].split("(")[0])
    except Exception:
        pass
    try:
        for l in open(os.path.join(SCRATCH, "work-needs-attention.txt")):
            f = l.split()   # date time branch ticket KIND detail...
            if len(f) >= 5 and f[4] == "BLOCKED":
                put(f[3], "blocked", "worker handed back BLOCKED - needs a person")
            elif len(f) >= 5 and f[4] in ("REVIEW_FAIL", "ERROR", "TIMEOUT", "UNCOMMITTED", "GATE_FAIL_ESCALATE", "GATE_FAIL_NO_SESSION", "CANCEL_PROPOSED"):
                put(f[3], "failed", f[4])
            elif len(f) >= 5 and f[4] == "NO_WORK":
                put(f[3], "stopped", "stopped or lost agent (NO_WORK)")
    except Exception:
        pass
    try:
        bm = dict(l.split("=", 1) for l in open(os.path.join(SCRATCH, "bulk-in-progress")).read().splitlines() if "=" in l)
        for b in bm.get("branches", "").split():
            put(_tk_of_branch(b), "testing", "bulk gate")
    except Exception:
        pass
    try:
        first = open(os.path.join(REPO, ".git", "MERGE_MSG")).read().splitlines()[0]
        m = re.search(r"task-t\d+", first)
        if m:
            put(_tk_of_branch(m.group(0)), "testing", "staged merge")
    except Exception:
        pass
    try:
        for l in open(os.path.join(SCRATCH, "merge-queue.txt")):
            put(_tk_of_branch(l.strip()), "queued", "merge queue")
    except Exception:
        pass
    working = set(); claims = {}
    try:
        claims = json.load(open(os.path.join(SCRATCH, "work-claims.json")))
        working = {tid for tid, c in claims.items() if c.get("state") == "running" and c.get("kind") == "work"}
    except Exception:
        pass
    # UP NEXT = the work runner's own dispatch order (ops/work-runner.py candidates()): todo, deps done,
    # not blocked_on / needs user|hardware / dispatch: manual, not claimed; user-requested first,
    # then priority, then number. Mirrored here rather than imported so the page has no runner dependency.
    claimed = set(claims)
    try:
        tl = tl or load_tasks_yaml()
        by = {t.get("id"): t for t in tl}
        pri = {"high": 0, "medium": 1, "normal": 2, "low": 3}
        def is_user(t):
            return bool(t.get("requested_by") or t.get("user_report") or str(t.get("found_by", ""))[:40].lower().startswith("user"))
        def num(tid):
            m = re.search(r"(\d+)", tid or ""); return int(m.group(1)) if m else 10**9
        cands = []
        for t in tl:
            tid = t.get("id")
            if t.get("status") != "todo" or tid in claimed or tid in st or tid in working:
                continue
            if t.get("needs") in ("user", "hardware") or t.get("blocked_on") or t.get("dispatch") == "manual":
                continue
            dps = t.get("depends_on") or t.get("deps") or []
            if any(by.get(d, {}).get("status") not in ("done", "cancelled") for d in dps if d in by):
                continue
            cands.append(t)
        cands.sort(key=lambda t: (not is_user(t), pri.get(t.get("priority", "normal"), 2), num(t.get("id"))))
        for i, t in enumerate(cands[:limit]):
            put(t["id"], "next", f"up next #{i + 1}")
    except Exception:
        pass
    return st, why


def task_graph(scope="frontier", show_done=True, show_todo=True, show_blocked=True, at=None, ms=None,
               keep_merging=True, keep_next=True, keep_failed=True, keep_queue=True, keep_review=True, open_ms=(),
               collapse_done=False):
    try:
        tl = load_tasks_yaml()      # mtime-cached; a fresh PyYAML parse per poll had the monitor at 65 % CPU (2026-09-22)
        if not tl:
            raise RuntimeError("docs/tasks.yaml did not load (see the dashboard's error line)")
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
    rt, rt_why = runtime_states(smap, tl)
    # Runtime-state filters, one per state, same semantics as the status filters: off hides those
    # nodes, on shows them whatever their status filter says.
    show_rt = {"testing": keep_merging, "queued": keep_queue, "review": keep_review, "next": keep_next, "failed": keep_failed, "stopped": keep_failed, "blocked": show_blocked}
    def passes(x):
        s = x.get("status")
        r = rt.get(x.get("id"))
        if r:
            return show_rt.get(r, True)
        if s == "cancelled": return False
        if s == "done": return show_done
        if s == "todo": return show_todo
        if s in ("blocked", "paused"): return show_blocked
        if s == "deferred": return scope == "all"
        return True  # in-progress, review, etc. always anchor
    keep = {tid for tid, r in rt.items() if tid in tasks and show_rt.get(r, True)}
    # "Opening" a milestone (click its node) shows EVERY ticket under it - done, todo, deferred,
    # cancelled excepted - regardless of scope and the status filters, until it is clicked again.
    opened = {x["id"] for x in tl if _nm_ms(x.get("milestone")) in set(open_ms) and x.get("status") != "cancelled"}
    anchors = {x["id"] for x in cand if passes(x)} | set(running) | keep
    # ALWAYS keep the dependency chain leading to any NORMAL anchor, whatever its status/filter
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
    # An OPENED milestone adds exactly its own tickets - no chain walk in either direction. The
    # walk above turned "open M2" into the whole graph (2026-09-22); edges to tickets outside the
    # drawn set are simply not drawn.
    for tid in opened:
        if tid not in nodes:
            nodes[tid] = node_for(tid)
    # COLLAPSE DONE (user, 2026-09-23): contract every done/cancelled node out of the drawn graph
    # while keeping reachability - an edge that ran through a chain of done tickets becomes ONE
    # edge from the nearest live ancestor to the live descendant, labelled "via N done". A done
    # ticket that is an anchor for another reason (a live agent, a runtime state) stays.
    collapsed = set()
    if collapse_done:
        collapsed = {tid for tid, x in nodes.items()
                     if x.get("status") in ("done", "cancelled") and tid not in running and not rt.get(tid)}
    eff_memo = {}
    def eff_preds(tid, _stack=()):
        """{live ancestor: min number of collapsed nodes between it and `tid`}."""
        if tid in eff_memo:
            return eff_memo[tid]
        out = {}
        for dp in deps(nodes[tid]):
            if dp not in nodes or dp in _stack:
                continue
            if dp in collapsed:
                for anc, via in eff_preds(dp, _stack + (tid,)).items():
                    if anc not in out or via + 1 < out[anc]:
                        out[anc] = via + 1
            else:
                out[dp] = 0   # a direct live edge beats any route through collapsed nodes
        eff_memo[tid] = out
        return out
    cls = {"in-progress": "inprog", "todo": "todo", "blocked": "blocked", "paused": "blocked",
           "review": "review", "deferred": "deferred", "done": "done", "cancelled": "done"}
    def label(x):
        done = x.get("status") in ("done", "cancelled")
        t = str(x.get("title", "")).translate(str.maketrans("", "", '"[]<>|`{}')).strip()[:24]
        tick = "✓ " if done else ""
        ms = x.get("milestone") or ""
        state = rt.get(x["id"])
        if state:
            word = {"failed": "FAILED", "testing": "IN THE GATE", "queued": "QUEUED", "review": "IN REVIEW", "next": "UP NEXT", "stopped": "STOPPED", "blocked": "BLOCKED"}[state]
            col = {"failed": "#FF6B57", "testing": "#FFC14D", "queued": "#F0A542", "review": "#5EE0C4", "next": "#C7B8FF", "stopped": "#8595A0", "blocked": "#E47B68"}[state]
            why = rt_why.get(x["id"], "")
            why = "" if why in ("bulk gate", "merge queue", "reviewer stage") else " · " + why
            t = f"<b style='color:{col};font-size:10px;letter-spacing:.08em'>{word}{why}</b><br/>" + t
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
             "classDef msnext fill:#181f24,stroke:#5A6973,color:#8595A0,stroke-dasharray:5 4;",
             "classDef failed fill:#5a1a12,stroke:#FF6B57,color:#FFD9D2,stroke-width:4px;",
             "classDef testing fill:#4a3608,stroke:#FFC14D,color:#FFF0C2,stroke-width:4px,stroke-dasharray:9 5;",
             "classDef queued fill:#33280c,stroke:#F0A542,color:#FFE3B0,stroke-width:3px,stroke-dasharray:4 4;",
             "classDef reviewing fill:#0f3a33,stroke:#5EE0C4,color:#D6FFF5,stroke-width:4px;",
             "classDef next fill:#2b2352,stroke:#C7B8FF,color:#EFEAFF,stroke-width:4px;",
             "classDef stopped fill:#15191c,stroke:#5A6973,color:#8595A0,stroke-dasharray:2 4;"]
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
    lines.append("classDef msopen fill:#3a2c0a,stroke:#FFD98a,color:#FFF0C2,stroke-width:5px;")
    for ms in ORDER:
        ts = by_ms.get(ms, [])
        d_ = sum(1 for t in ts if t.get("status") in ("done", "cancelled"))
        cnt = f"{d_}/{len(ts)}" if ts else "planned"
        if ms in set(open_ms):
            lines.append(f'MS_{ms}(["▼ {ms} · {cnt} · open"]):::msopen')
        else:
            lines.append(f'MS_{ms}(["{ms} · {cnt}"]):::{mscls[ms_status(ms)]}')
    # Mermaid styles an edge by its INDEX in order of definition (`linkStyle i,j stroke:…`), so
    # every edge below is counted as it is emitted.
    edge_n = 0
    chain = [m for m in ORDER if m != "MUI"]
    for a, b in zip(chain, chain[1:]):
        lines.append(f"MS_{a} --> MS_{b}"); edge_n += 1
    lines.append("MS_M1 --> MS_MUI"); edge_n += 1
    for nid, x in nodes.items():
        if nid in collapsed:
            continue
        c = {"failed": "failed", "testing": "testing", "queued": "queued", "review": "reviewing", "next": "next", "stopped": "stopped", "blocked": "blocked"}.get(rt.get(nid)) \
            or ("running" if nid in running else cls.get(x.get("status"), "done"))
        shape = {"failed": ('{{"', '"}}'), "testing": ('(["', '"])'), "queued": ('[["', '"]]'), "blocked": ('(["', '"])'),
                 "reviewing": ('>"', '"]'), "next": ('[/"', '"/]')}.get(c, ('["', '"]'))
        lines.append(f"{nid}{shape[0]}{label(x)}{shape[1]}:::{c}")
    # dependency edges (solid) — draw among all nodes in scope, not just from active tasks.
    # An edge whose BOTH ends belong to an open milestone is coloured in that milestone's colour
    # (user, 2026-09-23: "I want to know which nodes those are") — only those; an edge into or
    # out of the milestone keeps the default stroke.
    MS_PALETTE = ["#FFD98a", "#5EE0C4", "#C7B8FF", "#FF9F7A", "#8FD3FF", "#F0A542"]
    ms_colour = {m: MS_PALETTE[i % len(MS_PALETTE)] for i, m in enumerate(open_ms)}
    ms_edges = {m: [] for m in open_ms}
    for nid, x in nodes.items():
        if nid in collapsed:
            continue
        preds = eff_preds(nid) if collapse_done else {dp: 0 for dp in deps(x) if dp in nodes}
        for dp, via in preds.items():
            if via:
                lines.append(f'{dp} -. "via {via} done" .-> {nid}')
            else:
                lines.append(f"{dp} --> {nid}")
            m = norm_ms(x.get("milestone"))
            if m in ms_edges and norm_ms(nodes[dp].get("milestone")) == m:
                ms_edges[m].append(edge_n)
            edge_n += 1
    # membership links (dotted) — connect every task to its milestone node
    for nid, x in nodes.items():
        if nid in collapsed:
            continue
        ms = norm_ms(x.get("milestone"))
        if ms in ORDER and not (ms in set(open_ms) and x.get("status") in ("done", "cancelled")):
            lines.append(f"MS_{ms} -.-> {nid}"); edge_n += 1
    for m, idx in ms_edges.items():
        if idx:
            lines.append(f"linkStyle {','.join(map(str, idx))} stroke:{ms_colour[m]},stroke-width:3px")
    total = len(tl)
    done = sum(1 for x in tl if x.get("status") in ("done", "cancelled"))
    counts = {}
    for tid, sname in rt.items():
        if tid in tasks:   # ticket rows only; a branch-level line (task-planned CONFLICT) is not a ticket
            counts[sname] = counts.get(sname, 0) + 1
    return {"mermaid": "\n".join(lines), "active": len(active), "nodes": len(nodes),
            "total": total, "done": done, "runtime": counts}

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
.filters button.rt{font-weight:600;letter-spacing:.04em;font-size:11px}
.filters button.chipx{color:#FFF0C2;border-color:#FFD98a;background:#3a2c0a;font-weight:600}.filters button.chipx.all{color:var(--mut);border-color:var(--line);background:transparent;font-weight:400}
.filters button.rt-next.on{color:#C7B8FF;border-color:#C7B8FF;background:#2b2352}.filters button.rt-merging.on{color:#FFC14D;border-color:#FFC14D;background:#4a3608}
.filters button.rt-queue.on{color:#F0A542;border-color:#F0A542;background:#33280c}.filters button.rt-review.on{color:#5EE0C4;border-color:#5EE0C4;background:#0f3a33}
.filters button.rt-failed.on{color:#FF6B57;border-color:#FF6B57;background:#5a1a12}
.filters select{background:var(--bg);color:var(--txt);border:1px solid var(--line);border-radius:5px;font-size:12px;padding:2px 4px;margin-left:4px;cursor:pointer}
.legend{margin-left:auto;display:flex;gap:10px;font-size:11px;color:var(--dim);flex-wrap:wrap}
.legend i{display:inline-block;width:9px;height:9px;border-radius:2px;margin-right:4px;vertical-align:0}
.wrap{flex:1;min-height:0;overflow:hidden;position:relative;cursor:grab;touch-action:none;user-select:none;-webkit-user-select:none}
@keyframes pulse-red{0%,100%{filter:drop-shadow(0 0 3px #FF6B57)}50%{filter:drop-shadow(0 0 16px #FF6B57) drop-shadow(0 0 4px #FF6B57)}}
@keyframes ants{to{stroke-dashoffset:-28}}
@keyframes glow-violet{0%,100%{filter:drop-shadow(0 0 2px #C7B8FF)}50%{filter:drop-shadow(0 0 12px #C7B8FF)}}
@keyframes glow-teal{0%,100%{filter:drop-shadow(0 0 2px #5EE0C4)}50%{filter:drop-shadow(0 0 12px #5EE0C4)}}
.node.failed{animation:pulse-red 1.1s ease-in-out infinite}
.node.testing rect,.node.testing path,.node.testing polygon{animation:ants .9s linear infinite}
.node.testing{filter:drop-shadow(0 0 8px #FFC14D)}
.node.next{animation:glow-violet 1.8s ease-in-out infinite}
.node.reviewing{animation:glow-teal 1.8s ease-in-out infinite}
.legend i.hex{clip-path:polygon(25% 0,75% 0,100% 50%,75% 100%,25% 100%,0 50%);border-radius:0;width:16px}
.legend i.stad{border-radius:8px;width:18px}.legend i.sub{border-radius:0;width:16px;box-shadow:inset 3px 0 #0D1317,inset -3px 0 #0D1317}
.legend i.trap{clip-path:polygon(15% 0,85% 0,100% 100%,0 100%);border-radius:0;width:18px}.legend i.asym{clip-path:polygon(0 0,100% 0,80% 50%,100% 100%,0 100%);border-radius:0;width:16px}
@media (prefers-reduced-motion:reduce){.node.failed,.node.next,.node.reviewing,.node.testing rect,.node.testing path,.node.testing polygon{animation:none}}
.wrap svg text{user-select:none;-webkit-user-select:none;pointer-events:none}
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
<span class=filters>show: <button id=f-done>done</button><button id=f-todo>todo</button><button id=f-blocked>blocked</button><button id=f-collapse title="contract done/cancelled tickets out of the graph; a chain through them becomes one edge labelled 'via N done'">collapse done</button>
<button id=f-next class="on rt rt-next">UP NEXT</button><button id=f-merging class="on rt rt-merging">MERGING</button><button id=f-queue class="on rt rt-queue">IN QUEUE</button><button id=f-review class="on rt rt-review">IN REVIEW</button><button id=f-failed class="on rt rt-failed">FAILED</button></span>
<span class=filters>milestone: <select id=msfilter><option value="">all milestones</option></select></span>
<span class=filters id=openms></span>
<a href="/">← dashboard</a>
<span class=legend><span><i class=hex style="background:#FF6B57"></i>FAILED / redo (pulsing)</span><span><i class=stad style="background:#FFC14D"></i>IN THE GATE (moving dashes)</span><span><i class=sub style="background:#F0A542"></i>QUEUED</span><span><i class=asym style="background:#5EE0C4"></i>IN REVIEW</span><span><i class=trap style="background:#C7B8FF"></i>UP NEXT</span><span><i style="background:#FFD98a"></i>working now</span><span><i style="background:#F0A542"></i>in progress</span><span><i style="background:#A395E0"></i>todo</span><span><i style="background:#E47B68"></i>blocked</span><span><i style="background:#52C2AE"></i>review</span><span><i style="background:#2f5d4e"></i>✓ done</span><span><i style="background:#5A6973"></i>deferred</span></span></div>
<div class=wrap><div id=g></div></div>
<div class=hint>scroll = zoom · drag = pan · click a ticket for details · <b>click a milestone to open/close all its tickets</b> · solid arrow = prerequisite → task · dotted = milestone → its tasks</div>
<script src="https://cdnjs.cloudflare.com/ajax/libs/mermaid/10.9.1/mermaid.min.js"></script>
<script>
mermaid.initialize({startOnLoad:false,theme:'dark',securityLevel:'loose',maxEdges:20000,maxTextSize:5000000,flowchart:{curve:'basis',htmlLabels:true,nodeSpacing:34,rankSpacing:70},themeVariables:{fontSize:'13px',lineColor:'#5A6973'}});
let last='',scope='frontier',flt={done:false,todo:false,blocked:false},msFilter='';
// Open milestones live for THIS TAB only (sessionStorage): a persisted "M2 open" survived a reload
// on 2026-09-22 and read as "the map always shows everything". The chips in the top bar say what
// is open and close it.
let openMs=new Set(); try{ localStorage.removeItem('graph.openMs'); openMs=new Set(JSON.parse(sessionStorage.getItem('graph.openMs')||'[]')); }catch(e){}
function renderOpenChips(){ const el=document.getElementById('openms'); if(!el) return;
  // Same palette and order as task_graph()'s MS_PALETTE: the chip's border is the colour of that
  // milestone's internal dependency edges on the map.
  const PAL=["#FFD98a","#5EE0C4","#C7B8FF","#FF9F7A","#8FD3FF","#F0A542"];
  el.innerHTML=openMs.size?('open: '+[...openMs].map((m,i)=>`<button class="chipx" data-ms="${m}" title="close ${m} · its internal edges are this colour" style="border-color:${PAL[i%PAL.length]};box-shadow:inset 0 -2px 0 ${PAL[i%PAL.length]}">${m} ✕</button>`).join('')+`<button class="chipx all" id=closeall title="close all">close all</button>`):'';
  el.querySelectorAll('button[data-ms]').forEach(b=>b.onclick=()=>toggleMs(b.dataset.ms)); const ca=document.getElementById('closeall'); if(ca) ca.onclick=()=>{openMs.clear(); saveMs(); last=''; draw();}; }
function saveMs(){ try{sessionStorage.setItem('graph.openMs',JSON.stringify([...openMs]));}catch(e){} renderOpenChips(); }
function toggleMs(m){ if(openMs.has(m)) openMs.delete(m); else openMs.add(m); saveMs(); last=''; draw(); }
renderOpenChips();
document.getElementById('sc-frontier').onclick=()=>setScope('frontier');
document.getElementById('sc-all').onclick=()=>setScope('all');
function setScope(s){scope=s;document.getElementById('sc-frontier').classList.toggle('on',s==='frontier');document.getElementById('sc-all').classList.toggle('on',s==='all');last='';draw();}
['done','todo','blocked','next','merging','queue','review','failed'].forEach(k=>{ if(flt[k]===undefined) flt[k]=true; document.getElementById('f-'+k).onclick=()=>{flt[k]=!flt[k];document.getElementById('f-'+k).classList.toggle('on',flt[k]);last='';draw();};});
// "collapse done" is off by default (it removes nodes); it is remembered like the other filters.
if(flt.collapse===undefined) flt.collapse=false; document.getElementById('f-collapse').classList.toggle('on',flt.collapse);
document.getElementById('f-collapse').onclick=()=>{flt.collapse=!flt.collapse;document.getElementById('f-collapse').classList.toggle('on',flt.collapse);last='';draw();};
async function draw(){
 try{
  let q='/graph.json?scope='+scope; ['done','todo','blocked','next','merging','queue','review','failed'].forEach(k=>{ if(!flt[k]) q+='&'+k+'=0'; });
  if(flt.collapse) q+='&collapse=1';
  if(openMs.size) q+='&open='+encodeURIComponent([...openMs].join(','));
  const d=await (await fetch(q,{cache:'no-store'})).json();
  const hid=['done','todo','blocked','next','merging','queue','review','failed'].filter(k=>!flt[k]);
  const rt=d.runtime||{}; const rts=['failed','testing','queued','review','next'].filter(k=>rt[k]).map(k=>`${rt[k]} ${k}`).join(' · ');
  document.getElementById('sub').textContent=`${d.active} active · ${d.done}/${d.total} done · ${scope==='all'?'all tasks':'frontier'}${hid.length?' · hiding '+hid.join('/'):''}${rts?' · '+rts:''}${openMs.size?' · open: '+[...openMs].join(', '):''}`;
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
  gg.querySelectorAll('.node').forEach(n=>{ const m=(n.textContent||'').match(/T-\d+/); if(m) n.setAttribute('data-node-tid',m[0]);
    else { const mm=(n.id||'').match(/MS_([A-Za-z0-9]+)/) || (n.textContent||'').match(/^\s*(?:▼\s*)?(M[A-Za-z0-9]+)\s*·/); if(mm) n.setAttribute('data-node-ms',mm[1]); } });
}
// Pointer -> SVG user units via the screen CTM. The old code scaled dy by vb.h/r.height, which is
// only right when the graph is height-limited; a wide LR graph is width-limited under
// preserveAspectRatio=meet, so a full-screen vertical drag moved the graph a fraction (2026-09-22).
function svgPt(x,y){ const q=svgEl.createSVGPoint(); q.x=x; q.y=y; return q.matrixTransform(svgEl.getScreenCTM().inverse()); }
wrap.addEventListener('wheel',e=>{ e.preventDefault(); if(!vb||!svgEl)return;
  const p=svgPt(e.clientX,e.clientY);                       // zoom about the cursor: this point stays put
  const nw=Math.min(W*3,Math.max(W*0.012,vb.w*Math.exp(e.deltaY*0.0015))), k=nw/vb.w;
  vb.x=p.x-(p.x-vb.x)*k; vb.y=p.y-(p.y-vb.y)*k; vb.w=nw; vb.h=vb.h*k; setVB();
},{passive:false});
wrap.addEventListener('pointerdown',e=>{ e.preventDefault(); down=true; dragMoved=false; px=e.clientX; py=e.clientY; });
wrap.addEventListener('pointermove',e=>{ if(!down||!vb)return; const r=wrap.getBoundingClientRect(), dx=e.clientX-px, dy=e.clientY-py;
  if(!dragMoved&&Math.abs(dx)+Math.abs(dy)>3){ dragMoved=true; wrap.classList.add('grabbing'); try{wrap.setPointerCapture(e.pointerId);}catch(_){} }
  if(dragMoved){ const a=svgPt(px,py), b=svgPt(e.clientX,e.clientY); vb.x-=(b.x-a.x); vb.y-=(b.y-a.y); px=e.clientX; py=e.clientY; setVB(); } });
function endDrag(e){ if(down&&!dragMoved){ const n=e.target.closest('[data-node-tid]'); if(n&&window.openTicketModal) window.openTicketModal(n.getAttribute('data-node-tid'));
    const mnode=e.target.closest('[data-node-ms]'); if(mnode&&!n) toggleMs(mnode.getAttribute('data-node-ms')); }
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
    # stage.sh writes stage.log to its own scratchpad dir, which may differ from SCRATCH
    # (~/.hackriff-ops). Read from whichever candidate dir has the freshest stage.log.
    sdir = SCRATCH; _best = -1.0
    for d in (os.path.dirname(os.path.abspath(__file__)), SCRATCH):
        try:
            m = os.path.getmtime(os.path.join(d, "stage.log"))
            if m > _best:
                _best = m; sdir = d
        except Exception:
            pass
    lines = []
    try:
        with open(os.path.join(sdir, "stage.log")) as f:
            lines = [l.rstrip() for l in f.read().splitlines() if l.strip()][-16:]
    except Exception:
        pass
    try:
        alive = bool(subprocess.run(["pgrep", "-f", "stage.sh"], capture_output=True, text=True, timeout=3).stdout.strip())
    except Exception:
        alive = False
    def rd(n):
        try:
            return open(os.path.join(sdir, n)).read().strip()
        except Exception:
            return ""
    smoke = ""
    for l in reversed(lines):
        if "SMOKE OK" in l: smoke = "ok"; break
        if "SMOKE FAIL" in l: smoke = "fail"; break
    return {"alive": alive, "source": rd("hk-serve-source").replace("source: ", ""),
            "built": rd("hk-serve-built-commit"), "smoke": smoke, "lines": lines}

def _ms_of(tid):
    try:
        return next((t.get("milestone", "") for t in load_tasks_yaml() if t.get("id") == tid), "")
    except Exception:
        return ""


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
    # 4. WHICH tickets are in the current test run vs. ahead-of-main and waiting
    def _tk(s):
        m = re.search(r"t(?:ask-t)?0*(\d+)", s or "", re.IGNORECASE)
        return "T-" + m.group(1) if m else (s or "")
    testing = []
    if merging:
        heads = []
        try:
            heads = open(os.path.join(REPO, ".git", "MERGE_HEAD")).read().split()
        except Exception:
            pass
        for h in heads:
            brs = [b for b in sh(["git", "for-each-ref", "--points-at", h,
                                  "--format=%(refname:short)", "refs/heads/"], cwd=REPO).split()
                   if b and b != "main"]
            nm = brs[0] if brs else h[:7]
            testing.append({"branch": nm, "ticket": _tk(nm if brs else mmsg)})
        if not testing and mticket:
            testing.append({"branch": "(staged)", "ticket": mticket})
    # A BULK batch has no MERGE_HEAD: its branches are named in $HACKRIFF_OPS/bulk-in-progress for
    # exactly the window the gate runs. Every merge on 2026-09-22 was a bulk, and this panel stayed
    # blank through all of them.
    try:
        bm = dict(l.split("=", 1) for l in open(os.path.join(SCRATCH, "bulk-in-progress")).read().splitlines() if "=" in l)
        for b in bm.get("branches", "").split():
            if b not in {t["branch"] for t in testing}:
                testing.append({"branch": b, "ticket": _tk(b), "bulk": True, "started": bm.get("started", ""), "milestone": _ms_of(_tk(b))})
    except Exception:
        pass
    if not gate and any(t.get("bulk") for t in testing):
        gate = "bulk gate (%d branches)" % sum(1 for t in testing if t.get("bulk"))
        try:
            elapsed = time.time() - time.mktime(time.strptime(bm.get("started", ""), "%Y-%m-%d %H:%M:%S"))
        except Exception:
            pass
    tbranch = {t["branch"] for t in testing}
    ahead = []
    try:
        wl = sh(["git", "worktree", "list", "--porcelain"], cwd=REPO)
        for b in sorted({l[7:].replace("refs/heads/", "") for l in wl.splitlines() if l.startswith("branch ")}):
            if b == "main" or b in tbranch:
                continue
            try:
                n = int(sh(["git", "rev-list", "--count", "main..%s" % b], cwd=REPO).strip() or 0)
            except Exception:
                n = 0
            if n > 0:
                ahead.append({"branch": b, "ticket": _tk(b), "commits": n, "milestone": _ms_of(_tk(b))})
    except Exception:
        pass
    # What "ahead of main" MEANS depends on the work runner's claim: a worker still running (the
    # commit is its first, more may come), a branch in the reviewer stage, one it already queued,
    # or a branch nobody owns. The panel used to call all four "waiting" (2026-09-22).
    try:
        claims = json.load(open(os.path.join(SCRATCH, "work-claims.json")))
        for a in ahead:
            c = claims.get(a["ticket"] or "", {})
            st, kind = c.get("state"), c.get("kind")
            a["state"] = ("review" if st == "running" and kind == "review" else "working" if st == "running"
                          else "queued" if st == "queued" else st or "unowned")
    except Exception:
        for a in ahead:
            a["state"] = "unowned"
    gates_running = 0
    try:
        gates_running = len([1 for l in out.splitlines() if "just gate-merge" in l and " grep " not in l])
    except Exception:
        pass
    # 5. gate PHASE / test-progress / typical-total — CI/CD progress for the panel
    phase = ""; progress = ""; suites_done = []; typical_s = 0
    if gate:
        try:
            import glob as _g
            cscr = "/private/tmp/claude-501/-Users-daniellewis-hackriff/%s/scratchpad" % COORD
            logs = sorted(_g.glob(cscr + "/gate*.log"), key=os.path.getmtime, reverse=True)
            if logs and (time.time() - os.path.getmtime(logs[0]) < 180):
                with open(logs[0], "rb") as _f:
                    _f.seek(0, 2); _sz = _f.tell(); _f.seek(max(0, _sz - 20000))
                    gtail = _f.read().decode("utf-8", "replace")
                suites_done = re.findall(r"gate: just (\S+) took", gtail)
                pm = re.findall(r"\((\d+)/(\d+)\)", gtail)
                if pm:
                    progress = "%s/%s" % (pm[-1][0], pm[-1][1])
        except Exception:
            pass
        g = gate
        if "lint" in g: phase = "linting"
        elif "test-ui" in g: phase = "ui e2e"
        elif "acceptance" in g: phase = "acceptance"
        elif "nextest" in g or g.strip() == "just test":
            phase = ("running tests " + progress) if progress else "building + testing"
        elif "gate-merge" in g:
            if "test-ui-e2e" in suites_done: phase = "ui e2e"
            elif any("acceptance" in s for s in suites_done): phase = "acceptance"
            elif "lint" in suites_done: phase = ("running tests " + progress) if progress else "building + testing"
            else: phase = "linting"
        else: phase = g
    try:
        import json as _j, statistics as _st
        tf = os.path.expanduser("~/.hackriff-ops/gate-timings.jsonl")
        durs = []
        if os.path.exists(tf):
            for line in open(tf):
                try:
                    o = _j.loads(line)
                    if o.get("kind") == "gate_end" and o.get("class") not in ("py", "docs", "ui") and (o.get("seconds") or 0) > 60:
                        durs.append(o["seconds"])
                except Exception:
                    pass
        if durs:
            typical_s = int(_st.median(durs[-15:]))
    except Exception:
        pass
    if not typical_s:
        typical_s = 1600  # ~27 min baseline for a full crates/ gate
    # how long the CURRENT merge has been running = age of .git/MERGE_HEAD (survives between gate phases)
    merge_age_s = 0
    if merging:
        try:
            merge_age_s = int(time.time() - os.path.getmtime(os.path.join(REPO, ".git", "MERGE_HEAD")))
        except Exception:
            pass
    state = "merging" if merging else ("gating" if gate else "idle")
    return {"state": state, "msg": mmsg, "ticket": mticket,
            "gate": gate, "elapsed_s": elapsed, "queue": queue,
            "testing": testing, "ahead": ahead, "gates_running": gates_running,
            "phase": phase, "progress": progress, "typical_s": typical_s, "merge_age_s": merge_age_s}

_PRI_RANK = {"high": 0, "medium": 1, "normal": 2, "low": 3}

_GATE_START = re.compile(r"^\[(\d\d-\d\d \d\d:\d\d:\d\d)\] (?:GATE (\S+) \(just gate-merge|BULK gate \(just gate --base (\w+) over (\d+) merged branches)")
_GATE_END = re.compile(r"^\[(\d\d-\d\d \d\d:\d\d:\d\d)\] (GATE FAILED (\S+)|MERGED (\S+) ✓|BULK MERGED ✓ (.*)|BULK gate FAILED.*|MERGE STATE LOST.*|GIVE UP.*)")
_GATE_FAIL = re.compile(r"^\s+(?:TRY \d+ )?FAIL \[\s*([\d.]+)s\]\s*\S*\s*(\S+)\s+(\S+)\s*$")
_GATE_SUITE = re.compile(r"^gate: (just \S+) took (\d+)s \(exit (\d+)\)")
_GATE_PANIC = re.compile(r"panicked at ([^:]+:\d+):\d+:\s*$")

def last_gates(n=4):
    """The newest gate runs, from ops/merge-runner.log: outcome, duration, suites, the tests
    that failed with the first line of their panic, or the build error when no test ran.

    User, 2026-09-22: "Our merge gate failed again. I don't know what the results are, because
    the agent dashboard doesn't show me." The log has had every answer all along; this reads
    the last ~2 MB of it (a gate is ~0.3-1 MB) rather than making a person scroll it.
    """
    path = os.path.join(SCRATCH, "merge-runner.log")
    try:
        with open(path, "rb") as f:
            f.seek(0, 2); sz = f.tell(); f.seek(max(0, sz - 2_500_000))
            lines = f.read().decode("utf-8", "replace").splitlines()
    except Exception:
        return []
    gates, cur = [], None
    for i, l in enumerate(lines):
        m = _GATE_START.search(l)
        if m:
            cur = {"started": m.group(1), "branches": [m.group(2)] if m.group(2) else [], "bulk": bool(m.group(3)),
                   "base": m.group(3) or "", "suites": [], "fails": [], "triage": [], "error": "", "outcome": "running", "ended": ""}
            if cur["bulk"]:
                # the batch's branches are on the preceding BULK attempt line
                for back in range(i - 1, max(0, i - 40), -1):
                    bm = re.search(r"BULK attempt \(\d+\): (.*)$", lines[back])
                    if bm:
                        cur["branches"] = bm.group(1).split(); break
            gates.append(cur); continue
        if cur is None:
            continue
        s = _GATE_SUITE.search(l)
        if s:
            cur["suites"].append({"suite": s.group(1), "s": int(s.group(2)), "rc": int(s.group(3))}); continue
        fm = _GATE_FAIL.search(l)
        if fm:
            name = fm.group(3)
            if name not in [x["test"] for x in cur["fails"]]:
                cur["fails"].append({"test": name, "binary": fm.group(2), "s": float(fm.group(1)), "at": "", "msg": ""})
            continue
        pm = _GATE_PANIC.search(l)
        if pm and cur["fails"]:
            # the panic line names the test thread: attach to that failure, else the newest without one
            tgt = next((x for x in cur["fails"] if ("'" + x["test"].split("::")[-1] + "'") in l), None) \
                or next((x for x in reversed(cur["fails"]) if not x["at"]), None)
            if tgt is not None and not tgt["at"]:
                tgt["at"] = pm.group(1)
                nxt = next((lines[j].strip() for j in range(i + 1, min(i + 4, len(lines))) if lines[j].strip()), "")
                tgt["msg"] = nxt[:300]
            continue
        # The gate's own timing verdict (py/hkpy/gatediag.py), printed after each suite. The
        # structured copy in gate-timings.jsonl is the primary source (gate_timing below); this
        # is the fallback, and it is per SUITE where the record carries one verdict per run.
        if l.startswith("gate: CONTENDED") or l.startswith("gate: DEARER") or l.startswith("gate: timing ok"):
            cur.setdefault("timing", [])
            if l.strip() not in cur["timing"]:
                cur["timing"].append(l.strip()[:200])
            continue
        if l.startswith("[") and "TRIAGE:" in l:
            cur["triage"].append(l.split("] ", 1)[-1][:200])
            if "retry PASSED" in l:
                cur["retry_passed"] = True
            continue
        if not cur["error"] and re.match(r"^(error(\[E\d+\])?: |gate: FAILED )", l) and "test run failed" not in l and "recipe" not in l:
            cur["error"] = l.strip()[:300]
        e = _GATE_END.search(l)
        if e:
            cur["ended"] = e.group(1)
            end = e.group(2)
            # A gate whose retry PASSED but whose merge was then refused (the branch moved
            # mid-gate) is not a failed gate: the tests were green. Say what happened instead
            # of "failed" (user, 2026-09-23: the 21:33 task-gate-speed gate read as a red).
            if "MERGE STATE LOST" in end:
                cur["outcome"] = "not-merged"
            else:
                cur["outcome"] = "passed" if "MERGED" in end else "failed"
            if cur.get("retry_passed") and cur["outcome"] != "failed":
                cur["error"] = ""   # the first red was a flake the retry cleared
            cur["end_line"] = end[:160]
            cur = None
    for g in gates:
        try:
            t0 = time.mktime(time.strptime("2026-" + g["started"], "%Y-%m-%d %H:%M:%S"))
            t1 = time.mktime(time.strptime("2026-" + g["ended"], "%Y-%m-%d %H:%M:%S")) if g["ended"] else time.time()
            g["seconds"] = int(t1 - t0)
        except Exception:
            g["seconds"] = 0
        for k in ("suites", "fails", "triage"):
            g[k] = g[k][:12]
        g["timing"] = g.get("timing", [])[:6]
    return gates[-n:][::-1]

def gate_timing():
    """The newest gate's SELF-DIAGNOSIS: contended, or timing ok, and which crates.

    `py/hkpy/gatediag.py` folds one verdict per run into the `gate_end` record — whether crates
    the diff never touched ran dearer than their own history. A 36-minute gate and a 36-minute
    gate on a loaded box are the same number and different facts (docs/test-speed-review §1:
    untouched crates 18-79x while the diff's own new tests cost 0 s), so the dashboard says
    which one this was instead of leaving a person to guess from the total.
    """
    path = os.path.join(SCRATCH, "gate-timings.jsonl")
    try:
        with open(path, "rb") as f:
            f.seek(0, 2); sz = f.tell(); f.seek(max(0, sz - 400_000))
            lines = f.read().decode("utf-8", "replace").splitlines()
    except Exception:
        return None
    for line in reversed(lines):
        try:
            rec = json.loads(line)
        except Exception:
            continue
        if rec.get("kind") != "gate_end" or "contended" not in rec:
            continue
        return {
            "contended": bool(rec.get("contended")),
            "crates": [[c[0], c[1]] for c in (rec.get("contended_crates") or [])][:6],
            "dearer": [[c[0], c[1]] for c in (rec.get("dearer") or [])][:6],
            "max_untouched": rec.get("max_untouched_ratio"),
            "suite": rec.get("timing_suite") or "",
            "runs": rec.get("timing_baseline_runs") or 0,
            "load": (rec.get("loadavg") or [None])[0],
            "age_s": int(time.time() - float(rec.get("ts") or 0)) if rec.get("ts") else None,
        }
    return None

def latest_junit():
    """The newest gate's per-test record: $HACKRIFF_OPS/junit/<run-id>/<n>-<suite>-<profile>.xml,
    written by `just gate` after each nextest suite (py/hkpy/gate.py keep_junit). Per file: how
    many tests, which failed (with the first line of the failure), and the slowest ten with
    their `time` - the machine-readable answer to "why did this gate take 36 minutes".
    """
    root = os.path.join(SCRATCH, "junit")
    try:
        runs = sorted((d for d in os.listdir(root) if os.path.isdir(os.path.join(root, d))),
                      key=lambda d: os.path.getmtime(os.path.join(root, d)))
    except Exception:
        return None
    if not runs:
        return None
    import xml.etree.ElementTree as ET
    run = runs[-1]; files = []
    for fn in sorted(os.listdir(os.path.join(root, run))):
        if not fn.endswith(".xml"):
            continue
        try:
            tree = ET.parse(os.path.join(root, run, fn)).getroot()
        except Exception:
            continue
        cases = []
        for tc in tree.iter("testcase"):
            f = tc.find("failure") if tc.find("failure") is not None else tc.find("error")
            msg = ""
            if f is not None:
                msg = (f.get("message") or (f.text or "")).strip().splitlines()[0][:240] if (f.get("message") or f.text) else "failed"
            cases.append({"name": tc.get("name", ""), "class": tc.get("classname", ""),
                          "s": float(tc.get("time") or 0), "failed": f is not None, "msg": msg})
        files.append({"file": fn, "tests": len(cases),
                      "failures": [c for c in cases if c["failed"]][:20],
                      "slowest": sorted(cases, key=lambda c: -c["s"])[:10],
                      "total_s": round(sum(c["s"] for c in cases))})
    return {"run": run, "age_s": int(time.time() - os.path.getmtime(os.path.join(root, run))), "files": files}

def flake_top(n=5):
    """The flake ledger's worst offenders — `$HACKRIFF_OPS/flakes.json` (py/hkpy/flakes.py).

    The merge runner forgives a load flake on every single gate and forgets it, so the same
    spec can cost four gates in a day with nothing counting to two. The ledger counts; this
    shows the top of it, with which WAY each test went, because "passes alone" (a wait to make
    deterministic) and "fails alone" (a real defect) are opposite jobs.
    """
    try:
        with open(os.path.join(SCRATCH, "flakes.json"), encoding="utf-8") as fh:
            data = json.load(fh)
        tests = data.get("tests") or {}
    except Exception:
        return []
    rows = []
    for name, rec in tests.items():
        if not isinstance(rec, dict):
            continue
        rows.append({
            "test": str(name),
            "red": int(rec.get("red_in_gate") or 0),
            "recent": int(rec.get("recent_red") or 0),
            "pass_alone": int(rec.get("passed_alone") or 0),
            "fail_alone": int(rec.get("failed_alone") or 0),
            "loads": [x for x in (rec.get("loads") or [])][-4:],
            "last": rec.get("last_seen") or 0,
        })
    rows.sort(key=lambda r: (-r["red"], -r["recent"], r["test"]))
    return rows[:n]

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
        deps = t.get("deps") or t.get("depends_on") or []
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
    # Three places an agent's transcript can live (user, 2026-09-23: every modal said "no agent
    # transcript" while workers were busy): (1) a work-runner worker runs `claude -p` with its
    # WORKTREE as cwd, so its transcript is under the per-worktree project dir; (2) a subagent of
    # ANY session (coordinator or supervisor), matched by the ticket id in its launch prompt;
    # (3) the legacy coordinator-only path, now covered by (2).
    cands = []
    m = re.match(r"T-0*(\d+)$", tid)
    if m:
        cands += [(p, True) for p in glob.glob(f"{PROJ}--claude-worktrees-t{m.group(1)}/*.jsonl")]
    cands += [(p, False) for p in glob.glob(f"{PROJ}/*/**/*.jsonl", recursive=True)]
    for p, by_dir in cands:
        try:
            head, _, sz = head_tail(p)
        except Exception:
            continue
        if not by_dir and _tid_of_label(first_user(head)) != tid:
            continue
        mt = os.path.getmtime(p)
        if best is None or (sz, mt) > (best[0], best[1]):
            best = (sz, mt, p)
    if not best:
        # No transcript, but the work runner may still have a record: say what it knows.
        d = os.path.join(SCRATCH, "work", tid)
        bits = []
        try:
            hb = json.load(open(os.path.join(d, "handback.json")))
            bits.append(f"**Hand-back** ({hb.get('outcome')}): {str(hb.get('summary', ''))[:1500]}")
        except Exception:
            pass
        try:
            res = json.load(open(os.path.join(d, "out.json"))).get("result", "")
            if res:
                bits.append("**Worker's last words:** " + str(res)[-1500:])
        except Exception:
            pass
        try:
            c = json.load(open(os.path.join(SCRATCH, "work-claims.json"))).get(tid)
            if c:
                bits.append(f"**Runner claim:** state `{c.get('state')}`, branch `{c.get('branch')}`, model {c.get('model')}")
        except Exception:
            pass
        return {"id": tid, "markdown": "\n\n".join(bits), "file": "work-runner record", "size_kb": 0} if bits else None
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

def _registry_ticket(label):
    """Ticket recorded at spawn by .claude/hooks/register-agent.sh, matched on the prompt head."""
    try:
        head = (label or "")[:120]
        if not head:
            return None
        with open(os.path.join(SCRATCH, "agent-registry.jsonl")) as f:
            for line in f.readlines()[-400:][::-1]:
                try:
                    o = json.loads(line)
                except Exception:
                    continue
                if o.get("ticket") and (o.get("prompt_head") or "")[:120] == head:
                    return o["ticket"]
    except Exception:
        pass
    return None


def _tid_of_cwd(path):
    """The transcript's own cwd names the worktree: …/worktrees/rl-t740 or …/t513 → the ticket."""
    try:
        with open(path, "rb") as f:
            head = f.read(60000).decode("utf-8", "replace")
        # The cwd first; else the first worktree path the agent touches (a coordinator subagent
        # keeps the coordinator's cwd and `cd`s into .claude/worktrees/rl-t740 in its commands).
        m = re.search(r'"cwd"\s*:\s*"([^"]+)"', head)
        for text in ([m.group(1)] if m else []) + [head]:
            mm = re.search(r"worktrees/[A-Za-z-]*t0*(\d{2,4})(?:[/\s\"'&]|$)", text)
            if mm:
                return f"T-{mm.group(1)}"
    except Exception:
        pass
    return None


def agents(status_map):
    out = []
    titles = {t.get("id"): t.get("title", "") for t in load_tasks_yaml()}
    mstone = {t.get("id"): t.get("milestone", "") for t in load_tasks_yaml()}
    # EVERY live session and its subagents, not only the coordinator's (user, 2026-09-22: the
    # supervisor's triage/fix/merge/SDET agents were invisible here). A session is a top-level
    # transcript under PROJ; its subagents live in <session>/subagents/. The coordinator is the
    # id in $HACKRIFF_OPS/coordinator-session when that file exists (the COORD constant went
    # stale the first time the coordinator was relaunched), else the COORD constant; the
    # session whose subagent dir this monitor's own launcher used is the supervisor.
    coord_id = COORD
    try:
        coord_id = open(os.path.join(SCRATCH, "coordinator-session")).read().strip() or COORD
    except Exception:
        pass
    sessions = [p for p in glob.glob(f"{PROJ}/*.jsonl") if os.path.getmtime(p) > time.time() - 1800]
    role_of = {}
    for p in sorted(sessions, key=os.path.getmtime, reverse=True):
        sid = os.path.basename(p)[:-6]
        role = "coordinator" if sid == coord_id else ("supervisor" if os.path.isdir(f"{PROJ}/{sid}/subagents") else "session")
        role_of[sid] = role
        s = session_summary(p, role)
        if s: s["status"] = None; s["running"] = s["age_s"] < 180; s["session"] = sid[:8]; out.append(s)
    subs = [p for sid in role_of for p in glob.glob(f"{PROJ}/{sid}/subagents/*.jsonl") + glob.glob(f"{PROJ}/{sid}/**/*.jsonl", recursive=True)]
    subs = sorted({p for p in subs if os.path.getmtime(p) > time.time() - 1800}, key=os.path.getmtime, reverse=True)
    # A subagent quiet longer than this is treated as no longer running. 900 s, not 210: a
    # triage agent running one browser spec or a scoped nextest binary writes nothing to its
    # transcript for 5-10 minutes, and 210 s hid the supervisor's fix agent mid-run (2026-09-22).
    # Work-runner workers have pid liveness from the claims file and do not depend on this.
    ACTIVE = 900
    best = {}       # dedupe by task id, keep the freshest transcript
    for p in subs:
        s = session_summary(p, "agent")
        if not s: continue
        parent = p[len(PROJ) + 1:].split("/", 1)[0]
        s["parent"] = role_of.get(parent, "session")
        # No ticket in the label: name the agent by what it was asked to do.
        kw = re.search(r"\b(deflak\w*|SDET|hand-merge|merge|fix|review|audit|triage|capture)\b", s["label"], re.IGNORECASE)
        s["kind"] = (kw.group(1).lower() if kw else "agent")
        m = re.search(r"(?:task|fixing task|implementing task)\s+(T-\d+)", s["label"], re.IGNORECASE)
        if m:
            tid = m.group(1).upper()
        else:
            # Fallback: the launch prompt's wording changed, but the ticket id is
            # still in it somewhere — take the first T-### we see.
            m2 = re.search(r"\bT-\d+\b", s["label"], re.IGNORECASE)
            tid = m2.group(0).upper() if m2 else None
        if not tid:
            # The orchestrator knew at spawn time (user, 2026-09-23): the PreToolUse(Agent) hook
            # wrote {ticket, prompt_head, cwd} to agent-registry.jsonl; match by prompt head,
            # else read the ticket out of the transcript's own cwd (…/worktrees/rl-t740 → T-740).
            tid = _registry_ticket(s["label"]) or _tid_of_cwd(p)
        st = status_map.get(tid)
        if st in ("done", "cancelled"): continue     # merged already
        if s["age_s"] > ACTIVE: continue              # gone quiet: not actually running
        key = tid or p
        if key not in best or s["age_s"] < best[key]["age_s"]:
            s["name"] = tid or f"{s['parent']} · {s['kind']}"; s["status"] = st; s["running"] = True; s["title"] = titles.get(tid, ""); s["milestone"] = mstone.get(tid, "")
            if not tid:
                s["label"] = f"{s['parent']}'s agent · " + str(s.get("label", ""))[:100]
            best[key] = s
    # Workers launched by ops/work-runner.py run `claude -p` with the WORKTREE as cwd, so their
    # transcripts live under a per-worktree project dir (…-hackriff--claude-worktrees-t514), not
    # under the coordinator's. 2026-09-22: two workers were 6 min into real work and the panel
    # showed none. Liveness comes from the runner's claim (pid) first, transcript age second.
    try:
        claims = json.load(open(os.path.join(SCRATCH, "work-claims.json")))
    except Exception:
        claims = {}
    def _alive(pid):
        try:
            os.kill(int(pid), 0); return True
        except Exception:
            return False
    for d in glob.glob(f"{PROJ}--claude-worktrees-*"):
        m = re.match(r"t(\d+)$", d.rsplit("-worktrees-", 1)[1])
        tid = f"T-{m.group(1)}" if m else None
        if not tid or status_map.get(tid) in ("done", "cancelled"):
            continue
        files = [p for p in glob.glob(f"{d}/*.jsonl") if os.path.getmtime(p) > time.time() - 1800]
        if not files:
            continue
        s = session_summary(max(files, key=os.path.getmtime), "agent")
        if not s:
            continue
        c = claims.get(tid, {})
        alive = c.get("state") == "running" and _alive(c.get("pid"))
        # The runner's claim is the truth when it exists: a worker it has reaped (killed, done,
        # uncommitted, no-work) is NOT running, however fresh its transcript - four killed workers
        # showed as running for 210 s on 2026-09-22 while the user was asking for a full stop.
        if not alive and (c or s["age_s"] > ACTIVE):
            continue
        s["name"] = tid; s["status"] = status_map.get(tid); s["running"] = True; s["title"] = titles.get(tid, ""); s["milestone"] = mstone.get(tid, "")
        s["label"] = f"work-runner · {c.get('model') or 'claude -p'} · " + str(s.get("label", ""))[:80]
        best[tid] = s
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

def watchdog_box():
    """The last tick of ops/watchdog.py: who on this box owns the CPU, and what nothing owns.

    Read from the file, never recomputed here - the dashboard already costs 440 % CPU when it
    does its own scanning (2026-09-22), and the whole point of the watchdog being a separate
    process is that one thing builds the process table. A missing or stale file says so rather
    than showing nothing: "no watchdog" is itself the condition that let sixteen busy loops run
    for two hours unseen.
    """
    try:
        d = json.load(open(os.path.join(SCRATCH, "watchdog.json")))
    except Exception:
        return {"state": "absent"}
    age = time.time() - float(d.get("ts", 0))
    d["age_s"] = round(age)
    d["state"] = "stale" if age > 120 else "live"
    return d


def sysstats():
    if psutil is None:
        return {"cores": os.cpu_count() or 1, "per": [], "mem": {}, "box": watchdog_box()}
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
        "box": watchdog_box(),
    }

_GATHER_LOCK = threading.Lock()
_GATHER_CACHE = {"at": 0.0, "data": None}

def gather_cached(max_age=2.5):
    """One gather() per ~2.5 s, shared by every client. gather() costs ~3 s (ps, git per
    worktree, the board) and the server is threaded, so every poller used to start its own:
    on 2026-09-22 the process sat at 440 % CPU and 2.1 GB and stopped answering. Late
    threads wait on the lock and get the fresh copy instead of computing another.
    """
    with _GATHER_LOCK:
        if _GATHER_CACHE["data"] is not None and time.time() - _GATHER_CACHE["at"] < max_age:
            return _GATHER_CACHE["data"]
        data = gather()
        _GATHER_CACHE.update(at=time.time(), data=data)
        return data

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
    gates = last_gates()
    # A gate with no end line and no gate process is one that was killed (a stopped runner, a
    # reboot): say so rather than "running" for ever.
    if gates and gates[0]["outcome"] == "running" and not mg.get("gate"):
        # merge_status's ps scan can time out under load; ask once more before saying "killed".
        try:
            alive = subprocess.run(["pgrep", "-f", "just gate"], capture_output=True, text=True, timeout=3).stdout.strip()
        except Exception:
            alive = "?"
        if not alive:
            gates[0]["outcome"] = "killed"
    # The JUnit record belongs to the newest gate that RAN nextest; a later docs/board-only gate
    # writes none. Say which gate it came from so a passed gate is not read as still failing.
    junit = latest_junit()
    if junit and gates:
        junit["stale"] = gates[0]["outcome"] in ("passed", "running") and junit["age_s"] > gates[0].get("seconds", 0) + 120
    return {
        "now": time.strftime("%Y-%m-%d %H:%M:%S %Z"),
        "worktrees": wt, "tasks": tk, "log": git_log(),
        "coord": coord_pane(), "agents": ags, "sys": system_load(), "stage": stage_status(),
        "merge": mg, "queue": work_queue(smap, wt, ags, mg.get("ticket", "")),
        "gates": gates, "junit": junit,
        "timing": gate_timing(), "flakes": flake_top(),
        "budget": budget_status(),
    }

# ------------------------------------------------------------------------------
# /flow — pipeline throughput visibility (pipeline manager, 2026-09-23): landings/h
# with the rolling 6h/24h lines, per-hour occupancy, per-gate durations by class,
# red rate by cause, touchpoints, and the open experiment (if any). Reads through
# hkpy.flow / hkpy.experiment (py/hkpy/flow.py, py/hkpy/experiment.py) — never
# re-parses the ops logs itself. build_flow_panel() is a pure function of `ops`
# and `now` so it is the pytest target directly; the route only adds the cache.
# ------------------------------------------------------------------------------

def _flow_modules():
    import sys as _sys
    py_dir = os.path.join(REPO, "py")
    if py_dir not in _sys.path:
        _sys.path.insert(0, py_dir)
    from hkpy import flow as flow_mod, experiment as exp_mod
    return flow_mod, exp_mod


def _flow_jsonl_points(ops, since_ts, until_ts):
    """Raw flow.jsonl records in [since_ts, until_ts] — the few real ticks recorded so far
    (the file started 2026-09-23 ~15:30), overlaid on the series backfilled from the logs.
    A missing or garbage file degrades to an empty list, never an exception."""
    out = []
    try:
        with open(os.path.join(ops, "flow.jsonl"), encoding="utf-8") as fh:
            for line in fh:
                try:
                    r = json.loads(line)
                except ValueError:
                    continue
                ts = r.get("ts")
                if isinstance(ts, (int, float)) and since_ts <= ts <= until_ts:
                    out.append({"ts": ts, "landings_per_h_6h": r.get("landings_per_h_6h"),
                                "landings_per_h_24h": r.get("landings_per_h_24h")})
    except (FileNotFoundError, OSError):
        pass
    return out


def _landings_series(hourly_rows, since):
    """One point per hour: rolling 6h/24h landings/h, from flow.hourly()'s per-hour landed
    counts — this is the backfill; flow.jsonl (above) has too few ticks yet to stand alone."""
    from datetime import timedelta
    landed = [r["landed"] for r in hourly_rows]
    h0 = since.replace(minute=0, second=0, microsecond=0)
    out = []
    for i in range(len(landed)):
        t = h0 + timedelta(hours=i)
        w6 = landed[max(0, i - 5):i + 1]
        w24 = landed[max(0, i - 23):i + 1]
        out.append({"hour": hourly_rows[i]["hour"], "ts": t.timestamp(), "landed": landed[i],
                    "roll6": round(sum(w6) / len(w6), 2) if w6 else 0.0,
                    "roll24": round(sum(w24) / len(w24), 2) if w24 else 0.0})
    return out


def build_flow_panel(ops, now=None):
    """Everything /flow.json serves, as one pure function over `ops` — never raises: any
    failure (hkpy unimportable, unreadable/garbage logs) comes back as {"error": ...} so a
    broken panel never takes the rest of the dashboard down with it."""
    from datetime import datetime, timedelta
    now = now or datetime.now()
    at = now.strftime("%Y-%m-%d %H:%M")
    try:
        flow_mod, exp_mod = _flow_modules()
    except Exception as e:
        return {"error": f"hkpy.flow unavailable: {e}", "at": at}
    try:
        warmup_since = now - timedelta(hours=72)   # 48h shown + 24h so roll24 is full at hour 0
        hourly72 = flow_mod.hourly(ops, warmup_since, now)
        landings_all = _landings_series(hourly72, warmup_since)
        display_h0 = (now - timedelta(hours=48)).replace(minute=0, second=0, microsecond=0)
        landings = [p for p in landings_all if p["ts"] >= display_h0.timestamp()]
        flow_points = _flow_jsonl_points(ops, display_h0.timestamp(), now.timestamp())

        hourly24 = flow_mod.hourly(ops, now - timedelta(hours=24), now)
        gates48 = flow_mod.gate_rows(ops, now - timedelta(hours=48), now)
        gates24 = flow_mod.gate_rows(ops, now - timedelta(hours=24), now)
        closed24 = [g for g in gates24 if g["verdict"] in ("green", "red")]
        full24 = sorted(g["minutes"] for g in closed24 if g["class"] == "full" and g["minutes"])
        full_p50 = full24[len(full24) // 2] if full24 else None
        reds24 = [g for g in closed24 if g["verdict"] == "red"]
        causes = collections.Counter(g["cause"] if g["cause"] in ("real", "flake", "flake-then-real") else "other"
                                      for g in reds24)

        tp_all = flow_mod.touchpoints(ops, now - timedelta(hours=24), now)

        cur, m, checks = exp_mod.status_of(ops, now)
        if cur is None:
            experiment = {"open": False}
        else:
            metric = cur["metric"].split()[0]
            base = cur["baseline"].get(metric)
            val = (m or {}).get(metric)
            delta = None if base in (None, 0) or val is None else round((val - base) / base * 100, 1)
            experiment = {
                "open": True, "id": cur["id"], "hypothesis": cur["hypothesis"], "knobs": cur["knobs"],
                "opened": cur["opened"], "opened_ts": cur["ts"], "rollback": cur["rollback"],
                "gates_counted": (m or {}).get("gates"), "gates_target": cur["gates"],
                "hours_counted": (m or {}).get("hours"), "hours_target": cur["hours"],
                "metric": metric, "metric_now": val, "metric_baseline": base, "metric_delta_pct": delta,
                "baseline_landings_per_h_24h": cur["baseline"].get("landings_per_h_24h"),
                "guards": [{"ok": ok, "text": text} for ok, text in checks],
            }
        # Baseline horizontal line for the full-gate p50 chart: the open experiment's own
        # baseline when one is running (what it's being measured against), else this window's.
        baseline_full_p50 = (cur["baseline"].get("full_gate_p50_min") if cur else None)
        if baseline_full_p50 is None:
            baseline_full_p50 = full_p50

        return {
            "at": at, "ts": now.timestamp(),
            "landings": {"series": landings, "flow_jsonl": flow_points},
            "hourly_24h": hourly24,
            "gates_48h": gates48,
            "full_gate_p50_min": full_p50, "baseline_full_gate_p50_min": baseline_full_p50,
            "causes_24h": dict(causes), "reds_24h": len(reds24), "gates_24h": len(closed24),
            "touchpoints_24h": {"count": len(tp_all), "items": tp_all[-10:]},
            "experiment": experiment,
        }
    except Exception as e:
        return {"error": str(e), "at": at}


_FLOW_LOCK = threading.Lock()
_FLOW_CACHE = {"at": 0.0, "data": None}

def flow_panel_cached(ops, max_age=30.0):
    """One build_flow_panel() per ~30 s, shared by every client — hourly()/gate_rows()/
    touchpoints() each re-read merge-runner.log in full (~500k lines), so this is the hard
    cache the brief asks for rather than a per-poller cost."""
    with _FLOW_LOCK:
        if _FLOW_CACHE["data"] is not None and time.time() - _FLOW_CACHE["at"] < max_age:
            return _FLOW_CACHE["data"]
        # built in a child process (_child_json): the log parse's heap goes when the child exits
        data = _child_json(f"""
import importlib.util, json
spec = importlib.util.spec_from_file_location("mon", {os.path.join(REPO, "ops", "monitor.py")!r})
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
print(json.dumps(m.build_flow_panel({ops!r})))
""")
        _FLOW_CACHE.update(at=time.time(), data=data)
        return data

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
/* REACHABILITY RULE (user, 2026-09-24): nothing on this page may be unreachable. Every card can
   shrink (flex-shrink 1 - never an inline flex:0 0 auto, which is what hid #mergecard on 09-22 and
   Work trees / Merge queue / Recent commits on 09-24), keeps a floor so it never collapses to its
   border, and scrolls its own body (.bd) or itself; the column scrolls when the floors alone do not
   fit. py/tests/test_monitor_layout.py enforces it on every card. */
.col{display:flex;flex-direction:column;gap:10px;min-height:0;min-width:0;overflow-y:auto;overflow-x:hidden}
.card{background:var(--panel);border:1px solid var(--line);border-radius:9px;padding:10px 12px;display:flex;flex-direction:column;flex:0 1 auto;min-height:min(96px,100%);min-width:0;overflow:auto}
.card.fill{flex:1}
.card h2{font-size:11px;text-transform:uppercase;letter-spacing:.09em;color:var(--mut);margin:0 0 8px;display:flex;justify-content:space-between;flex:0 0 auto}
.card h2 em{font-style:normal;color:var(--dim)}
.bd{overflow:auto;min-height:0;flex:1}
.counts{display:flex;gap:6px;flex-wrap:wrap}
.chip{font:11px var(--mono);padding:2px 8px;border-radius:5px;border:1px solid var(--line);color:var(--mut)}
.chip.done{color:var(--teal)}.chip.in-progress{color:var(--amber)}.chip.blocked,.chip.paused{color:var(--coral)}.chip.todo{color:var(--lav)}
.chip.ms{color:var(--amber);border-color:rgba(240,165,66,.4);padding:1px 6px;letter-spacing:.04em}
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
.boxline{margin-top:9px;font:11px var(--mono);color:var(--mut);line-height:1.6;overflow-wrap:anywhere}
.boxline .ow{color:var(--teal)}.boxline .ow.hot{color:var(--amber)}
.boxline .un{color:var(--coral)}
.boxline .hd{color:var(--dim);letter-spacing:.06em;text-transform:uppercase;font-size:10px}
@media(max-width:1000px){
  body{overflow:auto;overflow-x:hidden}
  .app{height:auto;min-height:100vh;overflow:visible}
  .cols{grid-template-columns:1fr 1fr;flex:none}
  .col{overflow:visible}
  .card.fill{flex:none}
  .bd{max-height:60vh}
}
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
<div class=top><h1>hack<b>riff</b> · agents</h1><span class=pill><span class=dot></span><span id=st>live</span></span><span class=t id=now></span><span class=pill id=load></span><span class=pill id=merge title="Is the coordinator handling the merge queue?"></span><span class=pill id=budget title="Claude token budget. Fed from /usage; update: curl 'http://127.0.0.1:8901/budget?weekly=90&session=3'"></span><a class=maplink href="/worklog" title="What each role session reported at the end of every turn">work log ↗</a><a class=maplink href="/worklog#leverage" title="Open tickets ranked by what landing each releases (just task order)">leverage ↗</a><a class=maplink href="/terminal">terminal ↗</a><a class=maplink href="/graph">task map ↗</a><a class=maplink href="/burndown">burndown ↗</a><a class=maplink href="/perf">perf ↗</a><a class=maplink href="/flow">flow ↗</a><span class=t id=err></span><span class=counts id=counts></span></div>
<div class=cols>
  <div class=col>
    <div class="card fill"><h2>Agents <em id=agn></em></h2><div class=bd id=agents></div></div>
  </div>
  <div class=col>
    <div class="card" id=mergecard><h2>Merge queue <em id=mqn></em></h2><div class=bd id=mergeq></div></div>
    <div class="card" id=levcard><h2>Leverage <em><a class=maplink href="/worklog#leverage">all ↗</a></em></h2><div class=bd id=lev style="font-size:12px"></div></div>
    <div class="card" id=queuecard style="max-height:44%"><h2>Up next <em id=qn></em></h2><div class=bd id=queue></div></div>
    <div class="card fill"><h2>Work trees <em id=wtn></em></h2><div class=bd id=wts></div></div>
  </div>
  <div class=col>
    <div class="card" id=syscard style="flex-shrink:0.2"><h2>System <em id=sys-sub></em></h2>
      <div class="cpu-wrap"><div class="cores" id=cores></div></div>
      <div class="gauges">
        <div><div class="mem-lbl"><span>Memory</span><span id=mem-txt></span></div><div class="mem-bar"><i id=mem-fill></i></div></div>
        <div><div class="mem-lbl"><span>Disk free</span><span id=disk-txt></span></div><div class="mem-bar"><i id=disk-fill></i></div></div>
      </div>
      <div class=boxline id=boxline title="ops/watchdog.py: per-owner CPU, and anything no worker/gate/role/demo owns"></div>
    </div>
    <div class="card" style="max-height:52%"><h2>Tasks <em id=tkn></em></h2><div class="bd log" id=active></div></div>
    <div class="card fill"><h2>Recent commits <em>main</em></h2><div class="bd log" id=log></div></div>
    <div class="card" style="height:190px"><h2>Staging server <em id=stage-sub></em></h2><div class="bd log" id=stage></div></div>
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
  if(mg.state==='merging'&&!mg.gate){ mgEl.textContent='■ staged merge, NO gate running · '+dur(mg.merge_age_s||0)+' — needs `git merge --abort` + runner restart'; mgEl.style.color='#E47B68'; mgEl.title='MERGE_HEAD exists but no gate process: a killed gate left it (user 2026-09-22: read as a 59-minute gate)'; }
  else if(mg.state==='merging'){ mgEl.textContent='⇄ merging'+(mg.gate?' · gate '+dur(mg.elapsed_s):''); mgEl.style.color='#A395E0'; mgEl.title='Merging: '+(mg.msg||'?'); }
  else if(mg.state==='gating'){ mgEl.textContent='⚙ '+mg.gate+' · '+dur(mg.elapsed_s); mgEl.style.color='#F0A542'; mgEl.title='Gate running before merge'; }
  else { mgEl.textContent='idle'+(mg.queue?' · '+mg.queue+' in-progress':''); mgEl.style.color='#5A6973'; mgEl.title='No merge or gate running'; }
  // Merge queue panel: what's IN the current test run vs. ahead-of-main and waiting.
  {
    const testing=mg.testing||[], ahead=mg.ahead||[];
    const row=(t,tag,col)=>`<div style="padding:1px 0"><span style="color:${col}">${tag}</span> <b data-tid="${esc(t.ticket)}" style="cursor:pointer">${esc(t.ticket||t.branch)}</b> ${t.milestone?`<span class="chip ms">${esc(t.milestone)}</span> `:''}<span style="color:#5A6973">${esc(t.branch)}${t.commits?(' · '+t.commits+' commit'+(t.commits===1?'':'s')+' ahead of main'):''}</span></div>`;
    const warn=(mg.gates_running||0)>1?`<div style="color:#E47B68;margin-bottom:4px">⚠ ${mg.gates_running} gate-merges running at once — likely duplicate/colliding</div>`:'';
    const testCol=(mg.state==='gating'||mg.state==='merging')?'#F0A542':'#5A6973';
    const mage=mg.merge_age_s||mg.elapsed_s||0;
    const over=mg.typical_s&&mage>mg.typical_s;
    const gl=(mg.state==='merging'||mg.gate)?`<div style="margin-bottom:3px"><span style="color:${over?'#E47B68':'#F0A542'}">⏱ ${dur(mage)}</span> <span style="color:#8595A0">· ${esc(mg.phase||mg.gate||'staged (between phases)')}</span>${mg.typical_s?`<span style="color:#5A6973"> · ~${Math.round(mg.typical_s/60)}m typical</span>`:''}</div>`:'';
    const hdr=t=>`<div style="margin:6px 0 2px;color:#8595A0;font-size:11px;text-transform:uppercase;letter-spacing:.04em">${t}</div>`;
    const ts=testing.length?testing.map(t=>row(t,'⚙ in test',testCol)).join(''):'<div style="color:#5A6973">— nothing being tested —</div>';
    const TAG={working:['🔧 worker running','#52C2AE'],review:['🔍 in review','#A395E0'],queued:['⏳ queued for merge','#F0A542'],unowned:['· unowned branch','#8595A0']};
    const wt=ahead.length?ahead.map(t=>{const [tag,col]=TAG[t.state]||['⏳ '+(t.state||'waiting'),'#8595A0']; return row(t,tag,col);}).join(''):'<div style="color:#5A6973">— none waiting —</div>';
    // Last gate results (user, 2026-09-22): the outcome, the suites' durations, the tests that
    // failed with their panic line, or the build error - read from ops/merge-runner.log - and the
    // newest JUnit record's failures + slowest tests when the gate wrote one.
    const G=d.gates||[]; const mono='font-family:ui-monospace,Menlo,monospace;font-size:11px';
    const gateRow=g=>{
      const col=g.outcome==='passed'?'#52C2AE':g.outcome==='failed'?'#E47B68':'#F0A542';
      const who=g.bulk?`bulk · ${g.branches.length} branches`:esc(g.branches[0]||'?');
      const suites=(g.suites||[]).map(s=>`<span style="color:${s.rc?'#E47B68':'#8595A0'}">${esc(s.suite.replace('just ',''))} ${dur(s.s)}</span>`).join(' · ');
      const fails=(g.fails||[]).map(f=>`<div style="${mono};padding:1px 0 1px 12px;color:#E47B68">✗ ${esc(f.test)} <span style="color:#5A6973">${esc(f.binary)} · ${f.s.toFixed(1)}s</span>${f.at?`<div style="color:#8595A0;padding-left:14px">${esc(f.at)} — ${esc(f.msg)}</div>`:''}</div>`).join('');
      const err=(!g.fails.length&&g.error)?`<div style="${mono};padding:1px 0 1px 12px;color:#E47B68">${esc(g.error)}</div>`:'';
      const tri=(g.triage||[]).map(t=>`<div style="${mono};padding-left:12px;color:#A395E0">${esc(t)}</div>`).join('');
      return `<div style="padding:3px 0;border-top:1px solid #1e2830"><span style="color:${col};font-weight:600">${g.outcome==='running'?'⚙ running':g.outcome==='killed'?'■ killed (no gate process)':g.outcome==='passed'?'✓ passed':g.outcome==='not-merged'?'✓ tests passed · NOT merged (branch moved mid-gate; re-queued)':'✗ failed'}${g.retry_passed&&g.outcome!=='not-merged'?' <span style="color:#F0A542">(after one flake retry)</span>':''}</span> <span style="color:#8595A0">${esc(g.started)} · ${dur(g.seconds)} · ${who}</span>${g.bulk?`<div style="color:#5A6973;${mono}">${g.branches.map(esc).join(' ')}</div>`:''}<div style="color:#5A6973">${suites||'(no suite finished)'}</div>${fails}${err}${tri}</div>`;
    };
    // Only the NEWEST gate is shown in full; earlier ones collapse to one line each, their
    // failures behind a toggle - a failure that was fixed must not keep reading as current.
    const oneLine=g=>{const col=g.outcome==='passed'?'#52C2AE':g.outcome==='failed'?'#E47B68':'#8595A0'; const who=g.bulk?`bulk · ${g.branches.length}`:esc(g.branches[0]||'?'); const nf=(g.fails||[]).length; return `<details style="padding:2px 0;border-top:1px solid #1e2830"><summary style="cursor:pointer;color:#8595A0"><span style="color:${col}">${g.outcome}</span> ${esc(g.started)} · ${dur(g.seconds)} · ${who}${nf?` · ${nf} failed`:''}${(!nf&&g.error)?' · suite error':''}</summary>${gateRow(g)}</details>`;};
    const gates=G.length?gateRow(G[0])+G.slice(1).map(oneLine).join(''):'<div style="color:#5A6973">— no gate in the log tail —</div>';
    const J=d.junit; let ju='';
    if(J){
      const fl=J.files||[]; const nf=fl.reduce((a,f)=>a+f.failures.length,0), nt=fl.reduce((a,f)=>a+f.tests,0);
      const slow=fl.flatMap(f=>f.slowest.map(c=>({...c,file:f.file}))).sort((a,b)=>b.s-a.s).slice(0,8);
      const stale=J.stale?` <span style="color:#F0A542">· from an EARLIER gate (the newest ran no nextest)</span>`:'';
      const fails=J.stale?'':fl.flatMap(f=>f.failures.map(c=>`<div style="${mono};color:#E47B68;padding-left:12px">✗ ${esc(c.class)}::${esc(c.name)} <span style="color:#5A6973">${c.s.toFixed(1)}s</span><div style="color:#8595A0;padding-left:14px">${esc(c.msg)}</div></div>`)).join('');
      ju=hdr(`JUnit · run ${esc(J.run)} · ${dur(J.age_s)} ago · ${nt} tests · ${nf} failed`)+(J.stale?`<div style="color:#F0A542">${stale}</div>`:'')+fails
        +`<div style="color:#8595A0;margin-top:3px">slowest:</div>`+slow.map(c=>`<div style="${mono};padding-left:12px"><span style="color:#F0A542">${dur(c.s)}</span> ${esc(c.class)}::${esc(c.name)}</div>`).join('');
    }
    // Was the gate SLOW, or was the BOX slow? (py/hkpy/gatediag.py, from the gate_end record;
    // the per-suite line in the log is the fallback.) A 36-minute gate on a quiet box and one
    // on a loaded box are the same number and opposite facts, so the dashboard names which.
    const T=d.timing; let tm='';
    if(T){
      if(T.contended){
        const cr=(T.crates||[]).map(c=>`${esc(c[0])} ${Number(c[1]).toFixed(1)}x`).join(', ');
        tm=`<div style="color:#F0A542;padding:2px 0"><b>CONTENDED</b> <span style="color:#8595A0">${esc(T.suite||'')}</span> — ${cr||'untouched crates over 3x'} <span style="color:#5A6973">(untouched by the diff${T.load!=null?' · load '+Number(T.load).toFixed(1):''})</span></div>`;
      } else if(T.max_untouched!=null){
        tm=`<div style="color:#52C2AE;padding:2px 0">timing ok <span style="color:#5A6973">· max untouched crate ${Number(T.max_untouched).toFixed(1)}x · ${esc(T.suite||'')} vs ${T.runs} green run(s)</span></div>`;
      }
      if((T.dearer||[]).length)
        tm+=`<div style="color:#A395E0;padding:1px 0">DEARER — ${T.dearer.map(c=>`${esc(c[0])} ${Number(c[1]).toFixed(1)}x`).join(', ')} <span style="color:#5A6973">(touched: the change cost this)</span></div>`;
    }
    if(!tm){
      // Fallback: the line the gate printed, per suite, straight out of merge-runner.log.
      const tl=(G[0]&&G[0].timing)||[];
      if(tl.length) tm=tl.map(l=>`<div style="${mono};color:${l.startsWith('gate: CONTENDED')?'#F0A542':l.startsWith('gate: DEARER')?'#A395E0':'#52C2AE'}">${esc(l)}</div>`).join('');
    }
    // The flake ledger: which tests keep costing gates, and which WAY they go when re-run alone.
    const FL=d.flakes||[];
    const fl=FL.length?FL.map(f=>{
      const real=f.fail_alone>0, col=real?'#E47B68':'#F0A542';
      const how=real?(f.pass_alone?'both ways — triage in isolation':'fails alone — a real defect'):'passes alone — a load flake';
      const ld=(f.loads||[]).length?` · loads ${f.loads.map(x=>Number(x).toFixed(0)).join(',')}`:'';
      return `<div style="${mono};padding:1px 0"><span style="color:${col}">${f.red}x</span> <span style="color:#5A6973">(${f.recent} in 7d)</span> ${esc(f.test)}<div style="color:#8595A0;padding-left:14px">${how}${ld}</div></div>`;
    }).join(''):'';
    // Gate results FIRST (user, 2026-09-22: the waiting list grew past the viewport and hid them),
    // and the waiting list capped: the full set is in the Work trees card.
    const CAP=12; const wtShown=ahead.length>CAP?ahead.slice(0,CAP).map(t=>{const [tag,col]=TAG[t.state]||['⏳ '+(t.state||'waiting'),'#8595A0']; return row(t,tag,col);}).join('')+`<div style="color:#5A6973;padding:2px 0">… and ${ahead.length-CAP} more (see Work trees)</div>`:wt;
    const mq=$('#mergeq'); if(mq) mq.innerHTML=warn+gl+hdr('Last gate results')+tm+gates+ju
      +(fl?hdr('Flake ledger · tests that cost gates')+fl:'')
      +hdr('In the current test run')+ts+hdr('Ahead of main · not being tested')+wtShown;
    const mqn=$('#mqn'); if(mqn) mqn.textContent=testing.length+' in test · '+ahead.length+' waiting';
  }
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
  $('#agents').innerHTML=A.map(a=>`<div class=ag><div class=r1><span class="nm ${a.name==='coordinator'?'coordinator':''}"><span class="rdot ${a.running?'on':'off'}"></span>${/^T-\d/.test(a.name)?`<span class=tlink data-tid="${esc(a.name)}">${esc(a.name)}</span>`:esc(a.name)}${a.status?` <span class="chip ${a.status}">${a.status}</span>`:''}${a.milestone?` <span class="chip ms">${esc(a.milestone)}</span>`:''}${a.title?` <span class=agtitle>${esc(a.title)}</span>`:''}</span><span class=meta>${a.name==='coordinator'?'':dur(a.dur_s)+' · '}last ${dur(a.age_s)} ago</span></div>${a.label?`<div class=lbl>${esc(a.label)}</div>`:''}<div class=last>${esc(a.last)}</div></div>`).join('')||`<div class=lbl>${idleWhy}</div>`;
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
      qh+=`<div class="qrow ${r.merging?'merging':''}"><b>${tk_(r.id)}</b>${r.milestone?`<span class="chip ms">${esc(r.milestone)}</span>`:''}${tag}<span class=qt title="${esc(r.title)}">${esc(r.title)}</span></div>`;
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
// Leverage beside the merge queue (user, 2026-09-24: "put the Leverage top-5 and the ETA line on the
// MAIN page beside the queue, since that is where he looks") - the full panel is /worklog#leverage.
async function levTick(){ try{
  const [b,e]=await Promise.all([fetch('/taskorder.json',{cache:'no-store'}).then(r=>r.json()),
                                 fetch('/eta.json',{cache:'no-store'}).then(r=>r.json()).catch(()=>({}))]);
  const el=$('#lev'); if(!el) return;
  if(b.error){ el.innerHTML='<div style="color:var(--dim)">'+esc(String(b.error))+'</div>'; return; }
  const rows=(b.rows||[]).filter(r=>r.unblocks>0).slice(0,5);
  el.innerHTML=(e.line?`<div style="color:var(--amber);font:11.5px var(--mono);margin-bottom:4px">${esc(e.line)}</div>`:'')+
    (e.graph&&e.graph.line?`<div style="color:var(--amber);font:11.5px var(--mono);margin-bottom:4px" title="max expected landing over every todo/in-progress ticket; deferred and blocked excluded (py/hkpy/graphclear.py)">${esc(e.graph.line)}</div>`:'')+
    rows.map(r=>`<div style="display:flex;gap:8px;align-items:baseline"><span style="font:12px var(--mono);color:var(--amber);min-width:2.2em;text-align:right" title="open tickets transitively behind it">${esc(String(r.unblocks))}</span><span class=tlink data-tid="${esc(r.id)}" style="font:12px var(--mono)">${esc(r.id)}</span><span style="color:var(--dim);font:11px var(--mono)">${esc(Object.entries(r.downstream_milestones||{}).map(([k,v])=>k+' '+v).join(', '))}</span><span style="flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(r.title)}</span></div>`).join('')+
    `<div style="color:var(--dim);font:11px var(--mono);margin-top:3px">${esc(String(b.open))} open · frontier ${esc(String((b.frontier||[]).length))} ready now${(b.self_deps||[]).length?' · <span style="color:var(--coral)">self-dependency '+esc(b.self_deps.join(' '))+'</span>':''}</div>`;
}catch(x){} }
levTick(); setInterval(levTick,60000);
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
  renderBox(s.box||{});
 }catch(e){}
}
// "Box": who owns the CPU right now, from ops/watchdog.py's last tick. Unowned is drawn in red
// and never hidden — sixteen unowned busy loops ran for 2 h 18 m on 2026-09-22 because nothing
// displayed them. "no watchdog running" is itself shown, for the same reason.
function renderBox(b){
  const el=$('#boxline'); if(!el) return;
  if(b.state==='absent'){ el.innerHTML='<span class=un>⚠ no watchdog running</span> <span class=hd>— start ops/watchdog.py</span>'; return; }
  const ow=Object.entries(b.owners||{}).filter(([k,v])=>v.cpu>=1).slice(0,9)
    .map(([k,v])=>`<span class="ow${v.cpu>=150?' hot':''}">${esc(k)} ${Math.round(v.cpu)}%</span>`).join(' · ');
  const un=(b.unowned||[]).filter(u=>u.cpu>=20);
  const alarms=(b.alarms||[]).map(a=>`<span class=un>${esc(a.title)}</span>`).join(' · ');
  const over=(b.load||0)>(b.budget||1e9);
  el.innerHTML=`<div class=hd>Box · load <span style="color:${over?'#E47B68':'var(--mut)'}">${b.load}</span>/${b.budget} budget${b.state==='stale'?` · <span class=un>stale ${b.age_s}s</span>`:''}</div>`
    +`<div>${ow||'<span class=hd>idle</span>'}</div>`
    +(un.length?`<div class=un>unowned: ${un.map(u=>`${Math.round(u.cpu)}% pid ${u.pid} ${esc((u.cmd||'').slice(0,58))}`).join(' · ')}</div>`:'')
    +(alarms?`<div>${alarms}</div>`:'');
}
sysTick(); setInterval(sysTick,5000); setInterval(playFrame,1000);
</script></body></html>"""

# ------------------------------------------------------------------------------
# /perf — performance analytics page (data from ops/perf.py). Hand-drawn inline
# SVG, no chart library, same dark tokens as GRAPH_PAGE/PAGE. Two-thumb time
# slider + aggregate/split + per-session drilldown drive every chart.
# ------------------------------------------------------------------------------
PERF_PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>hackriff perf</title>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--teal:#52C2AE;--amber:#F0A542;--lav:#A395E0;--coral:#E47B68;--blue:#3a6ea5;--mono:"SFMono-Regular",Menlo,monospace}
*{box-sizing:border-box}html,body{height:100%;margin:0}
body{background:var(--bg);color:var(--txt);font:13px/1.5 -apple-system,system-ui,sans-serif;display:flex;flex-direction:column;overflow:hidden}
.top{display:flex;align-items:center;gap:14px;padding:8px 14px;border-bottom:1px solid var(--line);background:var(--panel);flex:0 0 auto}
.top span.nm b{color:var(--amber)}
a{color:var(--mut);text-decoration:none;border:1px solid var(--line);border-radius:6px;padding:3px 9px;font-size:12px}
a:hover{color:var(--txt)}
.sub{color:var(--dim);font:12px var(--mono);margin-left:auto}
.controls{display:flex;flex-wrap:wrap;gap:14px 22px;align-items:flex-end;padding:10px 14px;border-bottom:1px solid var(--line);background:var(--panel);flex:0 0 auto}
.ctl{display:flex;flex-direction:column;gap:6px;min-width:0}
.ctl>label{font:10px var(--mono);letter-spacing:.08em;text-transform:uppercase;color:var(--dim)}
.slider{position:relative;width:min(420px,58vw);height:22px;user-select:none;touch-action:none}
.slider .trk{position:absolute;top:9px;left:0;right:0;height:4px;background:var(--bg);border:1px solid var(--line);border-radius:3px}
.slider .rng{position:absolute;top:9px;height:4px;background:linear-gradient(90deg,var(--blue),var(--teal));border-radius:3px}
.slider .th{position:absolute;top:1px;width:13px;height:18px;margin-left:-6px;border-radius:4px;background:#1E2A33;border:1px solid var(--amber);cursor:grab;box-shadow:0 1px 4px rgba(0,0,0,.4)}
.slider .th:active{cursor:grabbing}
.rlabels{display:flex;justify-content:space-between;font:11px var(--mono);color:var(--mut);width:min(420px,58vw)}
.seg{display:flex;gap:2px;background:var(--bg);border:1px solid var(--line);border-radius:7px;padding:2px}
.seg button{border:0;background:transparent;color:var(--mut);padding:3px 10px;border-radius:5px;cursor:pointer;font-size:12px}
.seg button.on{background:#1E2A33;color:var(--txt)}
select{background:var(--bg);color:var(--txt);border:1px solid var(--line);border-radius:6px;font-size:12px;padding:4px 6px;max-width:300px;cursor:pointer}
.chk{display:flex;align-items:center;gap:6px;font-size:12px;color:var(--mut);cursor:pointer}
.chk input{accent-color:var(--amber)}
.wrap{flex:1;min-height:0;overflow:auto;padding:14px}
.grid{display:grid;grid-template-columns:1.5fr 1fr;gap:14px;align-items:start}
@media(max-width:900px){.grid{grid-template-columns:1fr}}
.card{background:var(--panel);border:1px solid var(--line);border-radius:9px;padding:12px 14px;min-width:0}
.card.wide{grid-column:1/-1}
.card h2{font-size:11px;text-transform:uppercase;letter-spacing:.09em;color:var(--mut);margin:0 0 10px;display:flex;justify-content:space-between;gap:8px}
.card h2 em{font-style:normal;color:var(--dim);text-transform:none;letter-spacing:0}
.chart{width:100%;min-height:20px}
svg{display:block;width:100%;height:auto}
.tlink{cursor:pointer}
.legend{display:flex;gap:12px;flex-wrap:wrap;font:11px var(--mono);color:var(--mut);margin-top:9px}
.legend i{display:inline-block;width:9px;height:9px;border-radius:2px;margin-right:4px;vertical-align:0}
table{width:100%;border-collapse:collapse;font:11.5px var(--mono);table-layout:fixed}
th,td{text-align:left;padding:4px 6px;border-top:1px solid var(--line);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
th{color:var(--dim);font-weight:400;text-transform:uppercase;letter-spacing:.06em;font-size:10px}
td.num{text-align:right;color:var(--amber)}
td.cmd{color:var(--mut)}
.mini-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(208px,1fr));gap:10px}
.mini{background:var(--bg);border:1px solid var(--line);border-radius:8px;padding:9px 10px;cursor:pointer;transition:border-color .15s}
.mini:hover{border-color:var(--dim)}
.mini .m1{display:flex;justify-content:space-between;gap:8px;align-items:baseline}
.mini .mt{color:var(--amber);font:12px var(--mono);font-weight:600}
.mini .mv{color:var(--txt);font:12px var(--mono)}
.mini .ml{color:var(--mut);font-size:11px;margin:3px 0 6px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.mini .mbar{height:8px;border-radius:4px;overflow:hidden;display:flex;background:#0a0f12}
.mini .mbar i{display:block;height:100%}
.mini .mf{display:flex;justify-content:space-between;font:10px var(--mono);color:var(--dim);margin-top:4px}
.tokrow{display:flex;gap:20px;flex-wrap:wrap;margin-bottom:12px}
.tokrow div{font:11px var(--mono);color:var(--mut)}
.tokrow b{color:var(--txt);font-size:15px;display:block;font-family:var(--mono)}
.empty{color:var(--dim);font-size:12px;padding:10px 0}
.bc{color:var(--dim);font:12px var(--mono);cursor:pointer;margin-bottom:10px}
.bc:hover{color:var(--txt)}
.errbox{color:var(--coral);font:12px var(--mono);padding:10px 0}
*{scrollbar-width:thin;scrollbar-color:transparent transparent}
::-webkit-scrollbar{width:8px;height:8px}::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:transparent;border-radius:4px}
:hover::-webkit-scrollbar-thumb{background:rgba(133,149,160,.4)}::-webkit-scrollbar-thumb:hover{background:rgba(133,149,160,.7)}
:hover{scrollbar-color:rgba(133,149,160,.4) transparent}
</style></head><body>
<div class=top><span class=nm>hack<b>riff</b> · perf</span><a href="/">← dashboard</a><a href="/graph">task map ↗</a><span class=sub id=sub>loading…</span></div>
<div class=controls>
  <div class=ctl><label>time range</label>
    <div class=slider id=sld><div class=trk></div><div class=rng id=sldRng></div><div class=th id=thLo></div><div class=th id=thHi></div></div>
    <div class=rlabels><span id=lblLo>—</span><span id=lblHi>—</span></div>
  </div>
  <div class=ctl><label>view</label>
    <div class=seg><button id=segAgg class=on>Aggregate</button><button id=segSplit>Split</button></div>
  </div>
  <div class=ctl><label>metric</label>
    <div class=seg id=segMetric><button data-m=time class=on>Time</button><button data-m=count>Count</button></div>
  </div>
  <div class=ctl><label>statistic</label>
    <div class=seg id=segStat><button data-s=sum class=on>Sum</button><button data-s=avg>Avg</button><button data-s=p50>Median</button><button data-s=p90>P90</button><button data-s=p95>P95</button></div>
  </div>
  <div class=ctl><label>unit</label>
    <div class=seg id=segUnit><button data-u=inv class=on>Per invocation</button><button data-u=agent>Per agent</button></div>
  </div>
  <div class=ctl><label>bars</label>
    <div class=seg id=segBars><button data-b=all class=on>Across all</button><button data-b=agent>Per agent</button></div>
  </div>
  <div class=ctl><label>session</label>
    <select id=agentSel><option value="">— all sessions —</option></select>
  </div>
  <div class=ctl><label>&nbsp;</label>
    <label class=chk><input type=checkbox id=allChk> include firehose sessions</label>
  </div>
</div>
<div class=wrap><div id=bc class=bc></div><div id=charts></div></div>
<script>
const $=s=>document.querySelector(s);
const esc=s=>String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const FM='ui-monospace,Menlo,monospace';
const C={txt:'#D5DEE2',mut:'#8595A0',dim:'#5A6973',teal:'#52C2AE',amber:'#F0A542',lav:'#A395E0',coral:'#E47B68',blue:'#3a6ea5',cold:'#8a5a18'};
const F={
  dur(s){ if(s==null) return '—'; s=Math.round(s); if(s<1) return '0s'; if(s<60) return s+'s'; if(s<3600){const m=Math.floor(s/60);return m+'m '+(s%60)+'s';} const h=Math.floor(s/3600);return h+'h '+Math.floor(s%3600/60)+'m';},
  tok(n){ if(n==null) return '—'; const a=Math.abs(n); if(a>=1e6) return (n/1e6).toFixed(2)+'M'; if(a>=1e3) return (n/1e3).toFixed(1)+'k'; return ''+Math.round(n);},
  count(n){ if(n==null) return '—'; return Math.round(n).toLocaleString(); },
  ratio(x){ if(x==null||!isFinite(x)) return '∞'; return x.toFixed(1)+'×'; },
  pct(p){ if(p==null) return '—'; return Math.round(p)+'%'; },
  date(ts){ if(!ts) return '—'; return new Date(ts*1000).toLocaleString(undefined,{month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'});}
};
// metric: time|count · stat: sum|avg|p50|p90|p95 · unit: inv|agent · bars: all|agent
const S={ frm:null, to:null, view:'agg', agent:null, all:0, loF:0, hiF:1,
          metric:'time', stat:'sum', unit:'inv', bars:'all', fam:null };
const STAT_LABEL={sum:'Sum',avg:'Avg',p50:'Median',p90:'P90',p95:'P95'};
// value for a family under the current (metric,stat,unit); everything is carried in the row
function famValue(f){
  if(S.metric==='count'){
    if(S.unit==='inv') return S.stat==='sum' ? (f.cnt_inv&&f.cnt_inv.sum||0) : 1; // 1 each per invocation
    return (f.cnt_agent||{})[S.stat] || 0;
  }
  const blk = S.unit==='agent' ? (f.dur_agent||{}) : (f.dur_inv||{});
  return blk[S.stat] || 0;
}
function famFmt(v){ return S.metric==='count' ? F.count(v) : F.dur(v); }
// cold/incremental split is a SUM-of-time concept only
function famSplitOn(){ return S.metric==='time' && S.stat==='sum'; }
function ctlDesc(){
  const u = S.metric==='count' && S.unit==='inv' ? 'per invocation' : (S.unit==='agent'?'per agent':'per invocation');
  return `${S.metric==='count'?'count':'time'} · ${STAT_LABEL[S.stat].toLowerCase()} · ${u}`;
}
let win={min_ts:null,max_ts:null}, agentsList=[], lastData=null;

function rect(x,y,w,h,fill){ return `<rect x="${(+x).toFixed(1)}" y="${(+y).toFixed(1)}" width="${Math.max(0,+w).toFixed(1)}" height="${h}" rx="2" fill="${fill}"/>`; }
function tsAt(f){ if(win.min_ts==null) return null; return win.min_ts + f*(win.max_ts-win.min_ts); }
function paintSlider(){
  const sld=$('#sld'), w=sld.clientWidth||1;
  const lo=Math.min(S.loF,S.hiF), hi=Math.max(S.loF,S.hiF);
  $('#thLo').style.left=(S.loF*w)+'px'; $('#thHi').style.left=(S.hiF*w)+'px';
  $('#sldRng').style.left=(lo*w)+'px'; $('#sldRng').style.width=((hi-lo)*w)+'px';
  $('#lblLo').textContent=F.date(tsAt(lo)); $('#lblHi').textContent=F.date(tsAt(hi));
}
let drag=null;
function fracAt(e){ const r=$('#sld').getBoundingClientRect(); return Math.max(0,Math.min(1,(e.clientX-r.left)/r.width)); }
$('#thLo').addEventListener('pointerdown',e=>{drag='lo';try{e.target.setPointerCapture(e.pointerId);}catch(_){}});
$('#thHi').addEventListener('pointerdown',e=>{drag='hi';try{e.target.setPointerCapture(e.pointerId);}catch(_){}});
window.addEventListener('pointermove',e=>{ if(!drag) return; const f=fracAt(e); if(drag==='lo') S.loF=f; else S.hiF=f; paintSlider(); });
window.addEventListener('pointerup',()=>{ if(!drag) return; drag=null; commitRange(); });
function commitRange(){
  const lo=Math.min(S.loF,S.hiF), hi=Math.max(S.loF,S.hiF);
  S.frm = lo>0.001 ? Math.floor(tsAt(lo)) : null;
  S.to  = hi<0.999 ? Math.ceil(tsAt(hi))  : null;
  load();
}
function query(){
  const p=[];
  if(S.frm!=null) p.push('frm='+S.frm);
  if(S.to!=null) p.push('to='+S.to);
  if(S.all) p.push('all=1');
  if(S.agent){ p.push('scope=agent'); p.push('agent='+encodeURIComponent(S.agent)); }
  return '/perf.json'+(p.length?'?'+p.join('&'):'');
}
function syncSeg(){ $('#segAgg').classList.toggle('on', S.view==='agg'||!!S.agent); $('#segSplit').classList.toggle('on', S.view==='split'&&!S.agent); }
function populateAgents(){
  const sel=$('#agentSel'), cur=S.agent||'';
  let h='<option value="">— all sessions —</option>';
  agentsList.forEach(a=>{ const lbl=(a.ticket?a.ticket+' · ':'')+String(a.label||a.id); h+=`<option value="${esc(a.id)}">${esc(lbl.slice(0,58))} · ${F.dur(a.total_s)}</option>`; });
  sel.innerHTML=h; sel.value=cur;
}
function showErr(e){ $('#charts').innerHTML=`<div class=errbox>perf error: ${esc(e)}</div>`; $('#sub').textContent='error'; }

async function load(){
  try{
    const data=await (await fetch(query(),{cache:'no-store'})).json();
    if(data.error){ showErr(data.error); return; }
    lastData=data;
    if(data.window && data.window.min_ts!=null) win=data.window;
    if(!S.agent) agentsList=data.agents||[];
    populateAgents(); paintSlider();
    const fam=(data.families||[]).length, ses=agentsList.length;
    $('#sub').textContent=`${ses} session${ses===1?'':'s'}${S.all?' · +firehose':''} · ${fam} families · ${F.date(win.min_ts)} → ${F.date(win.max_ts)}`;
    render(data);
  }catch(e){ showErr(e); }
}
function render(d){ if(S.view==='split' && !S.agent) renderSplit(); else renderAgg(d); }

function renderAgg(d){
  $('#bc').innerHTML = S.agent ? '‹ back to all sessions' : '';
  const fams=(d.families||[]);
  const perAgent=S.bars==='agent';
  // family focus for the per-agent view (default: top family)
  if(perAgent){ if(!S.fam || !fams.some(f=>f.stem===S.fam)) S.fam = fams.length?fams[0].stem:null; }
  const famHead = perAgent
    ? `Command families · per agent <em>${esc(ctlDesc())}</em>`
    : `Command families <em>${esc(ctlDesc())}${famSplitOn()?' · cold vs incremental':''}</em>`;
  const famPicker = perAgent
    ? `<select id=famPick style="max-width:180px">${fams.slice(0,40).map(f=>`<option value="${esc(f.stem)}"${f.stem===S.fam?' selected':''}>${esc(f.stem)}</option>`).join('')}</select>`
    : '';
  const famLegend = (!perAgent && famSplitOn())
    ? `<div class=legend><span><i style="background:${C.cold}"></i>cold build</span><span><i style="background:${C.amber}"></i>incremental</span><span><i style="background:${C.blue}"></i>other build</span><span><i style="background:${C.teal}"></i>non-build</span></div>`
    : '';
  $('#charts').innerHTML=`
  <div class=grid>
    <div class="card wide"><h2><span>${famHead}</span>${famPicker}</h2>
      <div id=cFam class=chart></div>${famLegend}
    </div>
    <div class=card><h2>Per-ticket time <em>top 25 · sum</em></h2>
      <div id=cTick class=chart></div>
      <div class=legend><span><i style="background:${C.teal}"></i>build</span><span><i style="background:${C.lav}"></i>test</span><span><i style="background:${C.amber}"></i>gate</span><span><i style="background:${C.blue}"></i>thinking</span><span><i style="background:${C.dim}"></i>other</span></div>
    </div>
    <div class=card><h2 title="Model time = wall-clock the model spent generating a turn (thinking + text + tool-call planning), clamped per turn. Not token count, not just <thinking> blocks.">Model time vs tool time</h2>
      <div id=cThink class=chart></div>
      <h2 style="margin-top:16px">Token spend</h2>
      <div id=cTok></div>
    </div>
    <div class="card wide"><h2>P90-slowest command types <em>ranked by 90th-percentile invocation time</em></h2><div id=cSlow></div></div>
  </div>`;
  if(perAgent) drawFamiliesPerAgent($('#cFam'), d.per_agent||[], S.fam, d.n_agents_total);
  else drawFamilies($('#cFam'), fams, d.n_agents_total);
  drawTickets($('#cTick'), d.tickets||[]);
  drawThinking($('#cThink'), d.thinking||{});
  $('#cSlow').innerHTML=p90Table(d.p90_slowest||[]);
  $('#cTok').innerHTML=tokenCard(d.tokens||{}, d.tickets||[]);
  const fp=$('#famPick'); if(fp) fp.onchange=e=>{ S.fam=e.target.value; render(lastData); };
}
function renderSplit(){
  $('#bc').innerHTML='';
  const A=(agentsList||[]).slice();
  if(!A.length){ $('#charts').innerHTML='<div class=empty>no sessions in range</div>'; return; }
  let h='<div class=mini-grid>';
  A.forEach(a=>{
    const tot=a.total_s||0, th=a.thinking_s||0, tl=a.tool_s||0, s=(th+tl)||1;
    const lbl=String(a.label||a.id);
    h+=`<div class=mini data-aid="${esc(a.id)}">
      <div class=m1><span class=mt>${esc(a.ticket||'—')}</span><span class=mv>${F.dur(tot)}</span></div>
      <div class=ml title="${esc(lbl)}">${esc(lbl.slice(0,54))}</div>
      <div class=mbar><i style="width:${(100*th/s).toFixed(1)}%;background:${C.blue}"></i><i style="width:${(100*tl/s).toFixed(1)}%;background:${C.amber}"></i></div>
      <div class=mf><span>${a.n_cmds} cmds · ${F.dur(th)} think</span><span>${F.tok(a.tokens_out)} tok</span></div>
    </div>`;
  });
  h+='</div>';
  $('#charts').innerHTML=h;
}

function drawFamilies(el, fams, nAgTot){
  // one bar per family, ordered by the selected (metric,stat,unit) value
  const rows=(fams||[]).map(f=>({f,v:famValue(f)})).filter(r=>r.v>0)
                       .sort((a,b)=>b.v-a.v).slice(0,14);
  const W=el.clientWidth||600;
  if(!rows.length){ el.innerHTML='<div class=empty>no commands in range</div>'; return; }
  const rh=30, lblW=Math.min(140,Math.max(78,W*0.24)), barX=lblW+10, metaW=Math.min(240,W*0.40), barW=Math.max(20,W-barX-metaW-6);
  const max=Math.max.apply(null,rows.map(r=>r.v).concat(1e-9)), H=8+rows.length*rh;
  const split=famSplitOn();
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  rows.forEach((r,i)=>{
    const f=r.f, y=6+i*rh, bh=16, by=y+3, w=barW*r.v/max;
    s+=`<text x="${lblW}" y="${by+12}" text-anchor="end" fill="${C.txt}" font-size="12" font-family="${FM}">${esc(f.stem)}</text>`;
    const cold=f.cold_sum||0, incr=f.incr_sum||0;
    if(split && (cold+incr)>0.05){
      const cw=barW*cold/max, iw=barW*incr/max, ow=Math.max(0,w-cw-iw);
      if(cw>0.3) s+=rect(barX,by,cw,bh,C.cold);
      if(iw>0.3) s+=rect(barX+cw,by,iw,bh,C.amber);
      if(ow>0.3) s+=rect(barX+cw+iw,by,ow,bh,C.blue);
    } else s+=rect(barX,by,w,bh,C.teal);
    const pct = nAgTot? ` (${F.pct(f.pct_agents)})` : '';
    const meta=`${famFmt(r.v)} · n=${f.n_invocations} · ${f.n_agents} agents${pct}`;
    s+=`<text x="${W-2}" y="${by+12}" text-anchor="end" fill="${C.mut}" font-size="10.5" font-family="${FM}">${esc(meta)}</text>`;
  });
  el.innerHTML=s+'</svg>';
}
function drawFamiliesPerAgent(el, perAgent, stem, nAgTot){
  // one bar per agent for the focused family, using that agent's own stat
  if(!stem){ el.innerHTML='<div class=empty>no families in range</div>'; return; }
  const rows=[];
  (perAgent||[]).forEach(a=>{ const fe=(a.families||[]).find(x=>x.stem===stem); if(!fe) return;
    let v; if(S.metric==='count') v=fe.count;
    else if(S.stat==='sum') v=fe.total_s;
    else if(S.stat==='avg') v=fe.count?fe.total_s/fe.count:0;
    else if(S.stat==='p50') v=fe.p50_s;
    else v=fe.p90_s; // p90 & p95 (p95 not tracked per agent) fall back to p90
    if(v>0) rows.push({a,fe,v});
  });
  rows.sort((x,y)=>y.v-x.v);
  const top=rows.slice(0,16), W=el.clientWidth||600;
  if(!top.length){ el.innerHTML=`<div class=empty>no agent ran ${esc(stem)} in range</div>`; return; }
  const rh=26, lblW=Math.min(150,Math.max(84,W*0.28)), barX=lblW+10, metaW=Math.min(150,W*0.26), barW=Math.max(20,W-barX-metaW-6);
  const max=Math.max.apply(null,top.map(r=>r.v).concat(1e-9)), H=8+top.length*rh+16;
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  top.forEach((r,i)=>{
    const y=6+i*rh, bh=14, by=y+3, w=barW*r.v/max;
    const lbl=String(r.a.ticket||r.a.label||r.a.agent_id||'—');
    const isT=/^T-\d/.test(lbl);
    s+=`<text x="${lblW}" y="${by+11}" text-anchor="end" fill="${isT?C.amber:C.mut}" font-size="11" font-family="${FM}"${isT?` class=tlink data-tid="${esc(lbl)}"`:''}>${esc(lbl.slice(0,18))}</text>`;
    s+=rect(barX,by,w,bh,C.lav);
    const meta=`${famFmt(r.v)} · ${r.fe.count}×`;
    s+=`<text x="${W-2}" y="${by+11}" text-anchor="end" fill="${C.mut}" font-size="10" font-family="${FM}">${esc(meta)}</text>`;
  });
  s+=`<text x="0" y="${H-1}" fill="${C.dim}" font-size="10" font-family="${FM}">${top.length} of ${rows.length} agents · ${nAgTot||'?'} ran any command</text>`;
  el.innerHTML=s+'</svg>';
}
function drawTickets(el, ticks){
  const rows=(ticks||[]).slice(0,25), W=el.clientWidth||500;
  if(!rows.length){ el.innerHTML='<div class=empty>no ticket time in range</div>'; return; }
  const rh=22, lblW=Math.min(112,Math.max(66,W*0.26)), barX=lblW+8, metaW=62, barW=Math.max(20,W-barX-metaW-4);
  const max=Math.max.apply(null,rows.map(t=>t.total_s).concat(1));
  const segs=[['build_s',C.teal],['test_s',C.lav],['gate_s',C.amber],['thinking_s',C.blue],['other_s',C.dim]];
  const H=6+rows.length*rh;
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  rows.forEach((t,i)=>{
    const y=5+i*rh, bh=13, by=y+2, id=String(t.ticket||t.label||''), isT=/^T-\d/.test(id);
    s+=`<text x="${lblW}" y="${by+11}" text-anchor="end" fill="${isT?C.amber:C.mut}" font-size="11" font-family="${FM}"${isT?` class=tlink data-tid="${esc(id)}"`:''}>${esc(id.slice(0,16))}</text>`;
    let x=barX;
    segs.forEach(sg=>{ const v=t[sg[0]]||0; if(v<=0) return; const w=barW*v/max; if(w>0.3) s+=rect(x,by,w,bh,sg[1]); x+=w; });
    s+=`<text x="${W-2}" y="${by+11}" text-anchor="end" fill="${C.mut}" font-size="10" font-family="${FM}">${F.dur(t.total_s)}</text>`;
  });
  el.innerHTML=s+'</svg>';
}
function drawThinking(el, th){
  const W=el.clientWidth||400, m=th.model_s||0, t=th.tool_s||0, tot=m+t, H=64, bh=22, by=6;
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  if(tot<=0){ el.innerHTML=s+`<text x="0" y="22" fill="${C.dim}" font-size="12" font-family="${FM}">no activity in range</text></svg>`; return; }
  const mw=W*m/tot, ratio=t>0?(m/t):Infinity;
  s+=rect(0,by,mw,bh,C.lav)+rect(mw,by,W-mw,bh,C.amber);
  s+=`<text x="0" y="${by+bh+18}" fill="${C.lav}" font-size="11.5" font-family="${FM}">▉ model ${F.dur(m)}</text>`;
  s+=`<text x="${W-2}" y="${by+bh+18}" text-anchor="end" fill="${C.amber}" font-size="11.5" font-family="${FM}">tool ${F.dur(t)} ▉</text>`;
  s+=`<text x="${W/2}" y="${by+bh+18}" text-anchor="middle" fill="${C.mut}" font-size="11" font-family="${FM}">ratio ${isFinite(ratio)?ratio.toFixed(2)+':1':'∞'}</text>`;
  el.innerHTML=s+'</svg>';
}
function p90Table(rows){
  if(!rows.length) return '<div class=empty>no commands in range</div>';
  let h='<table><colgroup><col><col style="width:74px"><col style="width:74px"><col style="width:70px"><col style="width:64px"><col style="width:72px"></colgroup>'
      +'<thead><tr><th>command</th><th>P90</th><th>avg</th><th title="P90 ÷ average — how much slower the tail is than the typical run">×slower</th><th>agents</th><th>% agents</th></tr></thead><tbody>';
  rows.forEach(r=>{
    h+=`<tr><td style="color:${C.txt}">${esc(r.stem)}</td>`
      +`<td class=num>${F.dur(r.p90_s)}</td>`
      +`<td class=num style="color:${C.mut}">${F.dur(r.avg_s)}</td>`
      +`<td class=num style="color:${C.coral}">${F.ratio(r.p90_over_avg)}</td>`
      +`<td class=num style="color:${C.mut}">${F.count(r.n_agents)}</td>`
      +`<td class=num style="color:${C.mut}">${F.pct(r.pct_agents)}</td></tr>`;
  });
  return h+'</tbody></table>';
}
function tokenCard(tok, tickets){
  const inn=tok.in||0, out=tok.out||0, cr=tok.cache_read||0, cc=tok.cache_creation||0;
  let h=`<div class=tokrow><div><b>${F.tok(out)}</b>output</div><div><b>${F.tok(inn)}</b>input</div><div><b>${F.tok(cr)}</b>cache read</div><div><b>${F.tok(cc)}</b>cache create</div></div>`;
  const rows=(tickets||[]).filter(t=>t.tokens_out>0).slice(0,12);
  if(!rows.length) return h+'<div class=empty>no per-ticket tokens</div>';
  const max=Math.max.apply(null,rows.map(t=>t.tokens_out).concat(1));
  h+='<div style="display:flex;flex-direction:column;gap:4px">';
  rows.forEach(t=>{ const id=String(t.ticket||t.label||''), isT=/^T-\d/.test(id);
    h+=`<div style="display:flex;align-items:center;gap:8px">
      <span style="width:76px;color:${C.mut};font:11px ${FM};overflow:hidden;text-overflow:ellipsis;white-space:nowrap"${isT?` class=tlink data-tid="${esc(id)}"`:''}>${esc(id.slice(0,13))}</span>
      <span style="flex:1;height:10px;background:#0a0f12;border-radius:5px;overflow:hidden"><i style="display:block;height:100%;width:${(100*t.tokens_out/max).toFixed(1)}%;background:${C.teal}"></i></span>
      <span style="width:52px;text-align:right;color:${C.amber};font:11px ${FM}">${F.tok(t.tokens_out)}</span>
    </div>`;
  });
  return h+'</div>';
}

$('#segAgg').onclick=()=>{ S.view='agg'; syncSeg(); render(lastData); };
$('#segSplit').onclick=()=>{ S.view='split'; if(S.agent){ S.agent=null; syncSeg(); load(); } else { syncSeg(); render(lastData); } };
// metric/stat/unit/bars are pure client re-renders (all distributions are in the payload)
function bindSeg(id, attr, key){
  const seg=$(id); if(!seg) return;
  seg.addEventListener('click',e=>{ const b=e.target.closest('button'); if(!b) return;
    S[key]=b.getAttribute(attr);
    seg.querySelectorAll('button').forEach(x=>x.classList.toggle('on', x===b));
    if(lastData) render(lastData);
  });
}
bindSeg('#segMetric','data-m','metric');
bindSeg('#segStat','data-s','stat');
bindSeg('#segUnit','data-u','unit');
bindSeg('#segBars','data-b','bars');
$('#agentSel').onchange=e=>{ S.agent=e.target.value||null; if(S.agent) S.view='agg'; syncSeg(); load(); };
$('#allChk').onchange=e=>{ S.all=e.target.checked?1:0; load(); };
document.addEventListener('click',e=>{
  if(e.target.closest('#bc') && S.agent){ S.agent=null; syncSeg(); load(); return; }
  const mini=e.target.closest('.mini'); if(mini){ S.agent=mini.getAttribute('data-aid'); S.view='agg'; syncSeg(); load(); }
});
let rz; window.addEventListener('resize',()=>{ clearTimeout(rz); rz=setTimeout(()=>{ paintSlider(); if(lastData) render(lastData); },150); });
load();
</script></body></html>"""

# ------------------------------------------------------------------------------
# /flow — pipeline throughput visibility. Same dark tokens as PAGE/PERF_PAGE,
# hand-drawn inline SVG (no chart library loaded anywhere in this file but
# mermaid, which is for /graph only). Data from /flow.json (build_flow_panel).
# ------------------------------------------------------------------------------
FLOW_PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>hackriff flow</title>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--teal:#52C2AE;--amber:#F0A542;--lav:#A395E0;--coral:#E47B68;--blue:#3a6ea5;--mono:"SFMono-Regular",Menlo,monospace}
*{box-sizing:border-box}html,body{height:100%;margin:0}
body{background:var(--bg);color:var(--txt);font:13px/1.5 -apple-system,system-ui,sans-serif;display:flex;flex-direction:column;overflow:hidden}
.top{display:flex;flex-wrap:wrap;align-items:center;gap:8px 14px;padding:8px 14px;border-bottom:1px solid var(--line);background:var(--panel);flex:0 0 auto}
.top span.nm b{color:var(--amber)}
a{color:var(--mut);text-decoration:none;border:1px solid var(--line);border-radius:6px;padding:3px 9px;font-size:12px}
a:hover{color:var(--txt)}
.sub{color:var(--dim);font:12px var(--mono);margin-left:auto}
.wrap{flex:1;min-height:0;overflow:auto;padding:14px}
.grid{display:grid;grid-template-columns:minmax(0,1.6fr) minmax(0,1fr);gap:14px;align-items:start}   /* minmax(0,..): a chart's width attr must not set the column's minimum */
@media(max-width:900px){.grid{grid-template-columns:minmax(0,1fr)}.wrap{padding:10px}}
.card{background:var(--panel);border:1px solid var(--line);border-radius:9px;padding:12px 14px;min-width:0}
.card.wide{grid-column:1/-1}
.card h2{font-size:11px;text-transform:uppercase;letter-spacing:.09em;color:var(--mut);margin:0 0 10px;display:flex;justify-content:space-between;gap:8px}
.card h2 em{font-style:normal;color:var(--dim);text-transform:none;letter-spacing:0}
.chart{width:100%;min-height:20px}
svg{display:block;width:100%;height:auto}
.tlink{cursor:pointer}
.legend{display:flex;gap:12px;flex-wrap:wrap;font:11px var(--mono);color:var(--mut);margin-top:9px}
.legend i{display:inline-block;width:9px;height:9px;border-radius:2px;margin-right:4px;vertical-align:0}
.legend i.dash{background:none;border-top:2px dashed var(--dim);width:12px;height:0;vertical-align:2px}
table{width:100%;border-collapse:collapse;font:11.5px var(--mono);table-layout:fixed}
th,td{text-align:left;padding:4px 6px;border-top:1px solid var(--line);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
th{color:var(--dim);font-weight:400;text-transform:uppercase;letter-spacing:.06em;font-size:10px}
td.num{text-align:right;color:var(--amber)}
.empty{color:var(--dim);font-size:12px;padding:10px 0}
.errbox{color:var(--coral);font:12px var(--mono);padding:10px 0}
.tp-row{padding:3px 0;border-top:1px solid var(--line);font:11.5px var(--mono);color:var(--mut);overflow-wrap:anywhere}
.tp-row:first-child{border-top:0}
.guard{padding:2px 0;font:11.5px var(--mono)}
.guard.ok{color:var(--teal)}.guard.bad{color:var(--coral)}.guard.nodata{color:var(--dim)}
.kv{display:flex;flex-wrap:wrap;gap:10px 18px;font:11.5px var(--mono);color:var(--mut);margin-bottom:8px}
.kv b{color:var(--txt)}
.rollback{font:11px var(--mono);color:var(--amber);background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:6px 8px;margin-top:8px;overflow-wrap:anywhere}
*{scrollbar-width:thin;scrollbar-color:transparent transparent}
::-webkit-scrollbar{width:8px;height:8px}::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:transparent;border-radius:4px}
:hover::-webkit-scrollbar-thumb{background:rgba(133,149,160,.4)}::-webkit-scrollbar-thumb:hover{background:rgba(133,149,160,.7)}
:hover{scrollbar-color:rgba(133,149,160,.4) transparent}
</style></head><body>
<div class=top><span class=nm>hack<b>riff</b> · flow</span><a href="/">← dashboard</a><a href="/perf">perf ↗</a><span class=sub id=sub>loading…</span></div>
<div class=wrap><div id=charts>loading…</div></div>
<script>
const $=s=>document.querySelector(s);
const esc=s=>String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const FM='ui-monospace,Menlo,monospace';
const C={txt:'#D5DEE2',mut:'#8595A0',dim:'#5A6973',teal:'#52C2AE',amber:'#F0A542',lav:'#A395E0',coral:'#E47B68',blue:'#3a6ea5'};
const CLASSCOL={full:C.teal,ui:C.lav,py:C.amber,docs:C.blue};
function classColor(k){ return CLASSCOL[k]||C.mut; }
const F={
  dur(m){ if(m==null) return '—'; if(m<60) return Math.round(m)+'m'; return (m/60).toFixed(1)+'h'; },
  n(x){ return x==null?'—':x; },
  pct(x){ return x==null?'—':(x>0?'+':'')+x+'%'; },
  date(ts){ if(!ts) return '—'; return new Date(ts*1000).toLocaleString(undefined,{month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}); }
};
function rect(x,y,w,h,fill){ return `<rect x="${(+x).toFixed(1)}" y="${(+y).toFixed(1)}" width="${Math.max(0,+w).toFixed(1)}" height="${h}" rx="2" fill="${fill}"/>`; }

// 1. landings/h line chart: rolling 6h/24h over the last 48h, flow.jsonl points overlaid,
// the open experiment's opening time as a vertical line and its baseline as a dashed line.
function drawLandings(el, d){
  const pts=(d.landings&&d.landings.series)||[];
  const W=el.clientWidth||700, H=170, padL=34, padR=8, padT=10, padB=18;
  if(!pts.length){ el.innerHTML='<div class=empty>no hourly data yet</div>'; return; }
  const xs=pts.map(p=>p.ts), t0=Math.min.apply(null,xs), t1=Math.max.apply(null,xs)||t0+3600;
  const vals=pts.flatMap(p=>[p.roll6,p.roll24]).filter(v=>v!=null);
  const exp=d.experiment&&d.experiment.open?d.experiment:null;
  const base=exp?exp.baseline_landings_per_h_24h:null;
  const maxV=Math.max.apply(null,vals.concat(base||0,0.1));
  const x=ts=>padL+(W-padL-padR)*((ts-t0)/Math.max(1,t1-t0));
  const y=v=>padT+(H-padT-padB)*(1-(v||0)/maxV);
  const line=(key,col)=>{
    let d2='', started=false;
    pts.forEach(p=>{ const v=p[key]; if(v==null){ started=false; return; } const cmd=started?'L':'M'; d2+=`${cmd}${x(p.ts).toFixed(1)},${y(v).toFixed(1)} `; started=true; });
    return d2.trim()?`<path d="${d2}" fill="none" stroke="${col}" stroke-width="2"/>`:'';
  };
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  // gridlines + y labels
  for(let i=0;i<=3;i++){ const v=maxV*i/3, yy=y(v); s+=`<line x1="${padL}" x2="${W-padR}" y1="${yy.toFixed(1)}" y2="${yy.toFixed(1)}" stroke="${C.dim}" stroke-opacity=".25"/>`; s+=`<text x="2" y="${(yy+3).toFixed(1)}" fill="${C.dim}" font-size="9.5" font-family="${FM}">${v.toFixed(1)}</text>`; }
  if(base!=null) s+=`<line x1="${padL}" x2="${W-padR}" y1="${y(base).toFixed(1)}" y2="${y(base).toFixed(1)}" stroke="${C.dim}" stroke-width="1.5" stroke-dasharray="4,3"/>`;
  if(exp&&exp.opened_ts) s+=`<line x1="${x(exp.opened_ts).toFixed(1)}" x2="${x(exp.opened_ts).toFixed(1)}" y1="${padT}" y2="${H-padB}" stroke="${C.amber}" stroke-width="1.5" stroke-dasharray="2,2"/>`;
  s+=line('roll24',C.blue)+line('roll6',C.teal);
  (d.landings.flow_jsonl||[]).forEach(p=>{ if(p.landings_per_h_24h!=null) s+=`<circle cx="${x(p.ts).toFixed(1)}" cy="${y(p.landings_per_h_24h).toFixed(1)}" r="2.6" fill="${C.amber}"/>`; });
  s+=`<text x="${padL}" y="${H-4}" fill="${C.dim}" font-size="10" font-family="${FM}">${F.date(t0)}</text>`;
  s+=`<text x="${W-padR}" y="${H-4}" text-anchor="end" fill="${C.dim}" font-size="10" font-family="${FM}">${F.date(t1)}</text>`;
  s+='</svg>';
  el.innerHTML=s;
}

// 2. per-hour, last 24h: dispatch-hours + gate occupancy as bars, landed count, red count.
function drawHourly(el, rows){
  if(!rows||!rows.length){ el.innerHTML='<div class=empty>no hourly data</div>'; return; }
  const W=el.clientWidth||700, rh=16, lblW=54, barX=lblW+12, barW=Math.max(20,W-barX-122);   // 122: room for '0 landed · 3 red' beside a full bar
  const H=8+rows.length*rh;
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  rows.forEach((r,i)=>{
    const y=4+i*rh, bh=11, by=y+1;
    s+=`<text x="${lblW}" y="${by+9}" text-anchor="end" fill="${C.mut}" font-size="10" font-family="${FM}">${esc(r.hour.split(' ')[1]||r.hour)}</text>`;
    const gateFrac=Math.max(0,Math.min(1,(r.gate_min||0)/60));
    s+=rect(barX,by,barW,bh,'#0a0f12');
    if(gateFrac>0) s+=rect(barX,by,barW*gateFrac,bh,C.amber);
    if(r.dispatch>0) s+=rect(lblW+4,by,5,bh,C.teal);   // its own mark left of the bar, not hidden under a full one
    const meta=`${r.landed||0} landed${r.red?` · ${r.red} red`:''}`;
    s+=`<text x="${W-2}" y="${by+9}" text-anchor="end" fill="${r.red?C.coral:C.mut}" font-size="10" font-family="${FM}">${esc(meta)}</text>`;
  });
  s+='</svg>';
  el.innerHTML=s;
  el.insertAdjacentHTML('afterend','<div class=legend><span><i style="background:'+C.amber+'"></i>gate occupancy (of the hour)</span><span><i style="background:'+C.teal+'"></i>dispatch happened</span></div>');
}

// 3. per-gate durations by class, last 48h; baseline full-gate p50 as a horizontal line.
function drawGates(el, gates, baseline){
  const rows=(gates||[]).filter(g=>g.minutes!=null);
  if(!rows.length){ el.innerHTML='<div class=empty>no closed gates in range</div>'; return; }
  const W=el.clientWidth||700, H=170, padL=34, padR=8, padT=10, padB=22;
  const maxM=Math.max.apply(null,rows.map(g=>g.minutes).concat(baseline||0,1));
  const x=i=>padL+(W-padL-padR)*(rows.length<=1?0.5:i/(rows.length-1));
  const y=v=>padT+(H-padT-padB)*(1-v/maxM);
  let s=`<svg viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" preserveAspectRatio="xMinYMin meet">`;
  for(let i=0;i<=3;i++){ const v=maxM*i/3, yy=y(v); s+=`<line x1="${padL}" x2="${W-padR}" y1="${yy.toFixed(1)}" y2="${yy.toFixed(1)}" stroke="${C.dim}" stroke-opacity=".25"/>`; s+=`<text x="2" y="${(yy+3).toFixed(1)}" fill="${C.dim}" font-size="9.5" font-family="${FM}">${Math.round(v)}</text>`; }
  if(baseline!=null) s+=`<line x1="${padL}" x2="${W-padR}" y1="${y(baseline).toFixed(1)}" y2="${y(baseline).toFixed(1)}" stroke="${C.mut}" stroke-width="1.5" stroke-dasharray="4,3"/>`;
  rows.forEach((g,i)=>{
    const cx=x(i), cy=y(g.minutes), red=g.verdict==='red', open=g.verdict==='open';
    const col = open? C.dim : (red?C.coral:classColor(g.class));
    s+=`<circle cx="${cx.toFixed(1)}" cy="${cy.toFixed(1)}" r="${red?4.2:3.2}" fill="${col}"${red?` stroke="${C.coral}" stroke-width="1.5" fill-opacity=".35"`:''}><title>${esc(g.start)} · ${g.class} · ${Math.round(g.minutes)}m · ${g.verdict}${g.cause?' · '+g.cause:''}</title></circle>`;
  });
  s+=`<text x="${padL}" y="${H-4}" fill="${C.dim}" font-size="10" font-family="${FM}">${esc((rows[0]||{}).start||'')}</text>`;
  s+=`<text x="${W-padR}" y="${H-4}" text-anchor="end" fill="${C.dim}" font-size="10" font-family="${FM}">${esc((rows[rows.length-1]||{}).start||'')}</text>`;
  s+='</svg>';
  el.innerHTML=s;
  const classes=[...new Set(rows.map(g=>g.class))];
  el.insertAdjacentHTML('afterend','<div class=legend>'+classes.map(k=>`<span><i style="background:${classColor(k)}"></i>${esc(k)}</span>`).join('')
    +'<span><i style="background:'+C.coral+'"></i>red</span>'+(baseline!=null?'<span><i class=dash></i>baseline p50 '+Math.round(baseline)+'m</span>':'')+'</div>');
}

// 4. red rate by cause, 24h.
function drawCauses(el, d){
  const c=d.causes_24h||{}; const order=['real','flake','flake-then-real','other'];
  const total=d.gates_24h||0, reds=d.reds_24h||0;
  let h=`<div class=kv><span>reds <b>${reds}/${total}</b></span></div>`;
  const max=Math.max.apply(null,order.map(k=>c[k]||0).concat(1));
  h+='<table><thead><tr><th>cause</th><th>count</th></tr></thead><tbody>';
  order.forEach(k=>{ const v=c[k]||0; const w=100*v/max;
    h+=`<tr><td style="color:${C.txt}">${k}</td><td class=num><div style="display:flex;align-items:center;gap:6px;justify-content:flex-end"><span style="width:${w.toFixed(0)}%;max-width:80px;height:8px;background:${k==='real'?C.coral:k==='other'?C.mut:C.amber};border-radius:4px;display:inline-block"></span>${v}</div></td></tr>`;
  });
  h+='</tbody></table>';
  el.innerHTML=h;
}

// 5. touchpoints, 24h.
function drawTouchpoints(el, d){
  const tp=d.touchpoints_24h||{count:0,items:[]};
  let h=`<div class=kv><span>count <b>${tp.count}</b></span></div>`;
  h += (tp.items&&tp.items.length) ? tp.items.map(t=>`<div class=tp-row>${esc(t)}</div>`).join('') : '<div class=empty>none in the last 24h</div>';
  el.innerHTML=h;
}

// 6. open experiment.
function drawExperiment(el, d){
  const e=d.experiment;
  if(!e||!e.open){ el.innerHTML='<div class=empty>no experiment open</div>'; return; }
  const delta=F.pct(e.metric_delta_pct);
  let h=`<div class=kv>
    <span>id <b>${esc(e.id)}</b></span>
    <span>opened <b>${esc(e.opened)}</b></span>
    <span>knobs <b>${esc((e.knobs||[]).join(', '))}</b></span>
    <span>gates <b>${F.n(e.gates_counted)}/${F.n(e.gates_target)}</b></span>
    <span>hours <b>${(e.hours_counted!=null?e.hours_counted.toFixed(1):'—')}/${F.n(e.hours_target)}</b></span>
  </div>
  <div style="color:${C.mut};margin-bottom:8px">${esc(e.hypothesis)}</div>
  <div class=kv><span>${esc(e.metric)} <b>${F.n(e.metric_now)}</b> vs baseline <b>${F.n(e.metric_baseline)}</b> <span style="color:${(e.metric_delta_pct||0)>=0?C.teal:C.coral}">${delta}</span></span></div>`;
  h += (e.guards||[]).map(g=>{ const cls=g.ok===true?'ok':g.ok===false?'bad':'nodata'; const mark=g.ok===true?'✓':g.ok===false?'✗ BROKEN':'—'; return `<div class="guard ${cls}">${mark} ${esc(g.text)}</div>`; }).join('');
  h += `<div class=rollback>rollback: ${esc(e.rollback)}</div>`;
  el.innerHTML=h;
}

async function load(){
  try{
    const d=await (await fetch('/flow.json',{cache:'no-store'})).json();
    if(d.error){ $('#charts').innerHTML=`<div class=errbox>flow error: ${esc(d.error)}</div>`; $('#sub').textContent='error'; return; }
    $('#sub').textContent=`as of ${esc(d.at)}`;
    $('#charts').innerHTML=`
    <div class=grid>
      <div class="card wide"><h2><span>Landings/h <em>rolling 6h (teal) / 24h (blue) · flow.jsonl ticks (amber dots)</em></span></h2><div id=cLand class=chart></div></div>
      <div class="card wide"><h2><span>Per hour, last 24h</span></h2><div id=cHourly class=chart></div></div>
      <div class="card wide"><h2><span>Per-gate durations by class, last 48h</span></h2><div id=cGates class=chart></div></div>
      <div class=card><h2>Red rate by cause <em>24h</em></h2><div id=cCauses></div></div>
      <div class=card><h2>Touchpoints <em>24h</em></h2><div id=cTouch></div></div>
      <div class="card wide"><h2>Open experiment</h2><div id=cExp></div></div>
    </div>`;
    drawLandings($('#cLand'), d);
    drawHourly($('#cHourly'), d.hourly_24h||[]);
    drawGates($('#cGates'), d.gates_48h||[], d.baseline_full_gate_p50_min);
    drawCauses($('#cCauses'), d);
    drawTouchpoints($('#cTouch'), d);
    drawExperiment($('#cExp'), d);
  }catch(e){ $('#charts').innerHTML=`<div class=errbox>fetch error: ${esc(e)}</div>`; $('#sub').textContent='error'; }
}
load(); setInterval(load,30000);
let rz; window.addEventListener('resize',()=>{ clearTimeout(rz); rz=setTimeout(load,150); });
</script></body></html>"""

def _child_json(code, timeout=90):
    """Run a heavy build in a short-lived child Python and return its JSON. The dashboard is a
    long-lived process, and each board parse (1.9 MB YAML) or flow build (merge-runner.log, ~500k
    lines) left ~6 MB of heap it never returned: measured 2026-09-24, RSS 470 -> 667 MB in 19 min,
    the watchdog's 1536 MB ceiling in ~1.5 h (the 2026-09-22 dashboard reached 2.1 GB and stopped
    answering). A child's memory goes back to the OS when it exits. A subprocess, not a fork: this
    is a threaded server, and a forked child can deadlock on a lock another thread held."""
    import sys as _sys
    out = subprocess.run([_sys.executable, "-c", code], cwd=REPO, capture_output=True, text=True, timeout=timeout,
                         env=dict(os.environ, PYTHONPATH=os.path.join(REPO, "py"), HACKRIFF_OPS=SCRATCH))
    if out.returncode != 0:
        raise RuntimeError((out.stderr or "child failed").strip().splitlines()[-1][:300])
    return json.loads(out.stdout)


_TASKORDER = {"t": 0.0, "v": None}
_ETA = {"t": 0.0, "v": None}


def eta_cached(max_age=300.0):
    """The digest's ETA line (hkpy.flow.eta_line: queue clears / top Leverage ticket lands), for the
    main page's Leverage card, plus the open-graph line (hkpy.graphclear) - built in a child
    (_child_json), cached 5 min: the graph estimate reads the board and the runner logs."""
    if _ETA["v"] is not None and time.time() - _ETA["t"] < max_age:
        return _ETA["v"]
    v = _child_json(f"""
import json
from datetime import datetime
from hkpy import flow
now = datetime.now()
s = flow.summary({SCRATCH!r}, now)
print(json.dumps({{"line": flow.eta_line({SCRATCH!r}, now, {REPO!r}), "graph": flow.open_graph({SCRATCH!r}, now, s, repo={REPO!r})}}))
""")
    _ETA.update(t=time.time(), v=v)
    return v


_FIXES = {"t": 0.0, "v": None}


def fixes_cached(max_age=60.0):
    """Why tickets were handed back for a fix run (hkpy.fixes.summary; user, 2026-09-24) - the
    /worklog "Fix runs" card. Built in a child (_child_json), cached 60 s."""
    if _FIXES["v"] is not None and time.time() - _FIXES["t"] < max_age:
        return _FIXES["v"]
    v = _child_json(f"""
import json
from hkpy import fixes
print(json.dumps(fixes.summary({SCRATCH!r})))
""")
    _FIXES.update(t=time.time(), v=v)
    return v


def taskorder_cached(max_age=60.0):
    """`hkpy.taskorder.analyse` over main's COMMITTED board (the bulk marker's base while a batch
    gates - never the provisional tip), built in a child process (_child_json) and cached 60 s."""
    if _TASKORDER["v"] is not None and time.time() - _TASKORDER["t"] < max_age:
        return _TASKORDER["v"]
    v = _child_json(f"""
import json
from hkpy import taskorder
tasks, ref = taskorder.committed_tasks({REPO!r}, {SCRATCH!r})
a = taskorder.analyse(tasks)
keep = ("id", "title", "status", "milestone", "depth", "gate", "unblocks", "value", "downstream_milestones", "blocked")
print(json.dumps({{"board": ref, "formula": a["formula"], "open": a["open"], "rows": [{{k: r[k] for k in keep}} for r in a["rows"]],
                  "groups": a["groups"], "frontier": a["frontier"], "cycle": a["cycle"], "self_deps": a["self_deps"]}}))
""")
    _TASKORDER.update(t=time.time(), v=v)
    return v


class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        # Leverage (py/hkpy/taskorder.py, `just task order`): which open tickets release the most
        # when they land, over main's committed board - the panel on /worklog beside the role logs.
        if self.path.startswith("/eta.json"):
            try:
                body = json.dumps(eta_cached()).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": f"{type(e).__name__}: {e}"}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/fixes.json"):
            try:
                body = json.dumps(fixes_cached()).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": f"{type(e).__name__}: {e}"}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/taskorder.json"):
            try:
                body = json.dumps(taskorder_cached()).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": f"{type(e).__name__}: {e}"}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        # Role work log (ops/worklog.py): each role session's end-of-turn report, for the user to read.
        if self.path.startswith("/worklog.json"):
            try:
                import sys as _sys
                if OPSDIR not in _sys.path:
                    _sys.path.insert(0, OPSDIR)
                import worklog
                body = json.dumps(worklog.build()).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": f"{type(e).__name__}: {e}", "roles": []}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/worklog"):
            import sys as _sys
            if OPSDIR not in _sys.path:
                _sys.path.insert(0, OPSDIR)
            import worklog
            body = worklog.PAGE.encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
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
            keep_merging = "merging=0" not in self.path
            keep_next = "next=0" not in self.path
            keep_failed = "failed=0" not in self.path
            keep_queue = "queue=0" not in self.path
            keep_review = "review=0" not in self.path
            _oq = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query).get("open", [""])[0]
            open_ms = tuple(m for m in _oq.split(",") if m)
            collapse_done = "collapse=1" in self.path
            try:
                body = json.dumps(task_graph(scope, show_done, show_todo, show_blocked, keep_merging=keep_merging, keep_next=keep_next, keep_failed=keep_failed, keep_queue=keep_queue, keep_review=keep_review, open_ms=open_ms, collapse_done=collapse_done)).encode(); self.send_response(200)
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
        if self.path.startswith("/burndown.json"):
            try:
                import burndown
                body = json.dumps(burndown.series(REPO, os.path.join(SCRATCH, "burndown-cache.json"))).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": str(e), "rows": []}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Cache-Control", "no-store"); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/burndown"):
            import burndown
            body = burndown.PAGE.encode(); self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Cache-Control", "no-store"); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
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
        if self.path.startswith("/perf.json"):
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            def _qi(k):
                v = q.get(k, [None])[0]
                if v in (None, ""):
                    return None
                try:
                    return int(float(v))
                except Exception:
                    return None
            frm = _qi("frm"); to = _qi("to")
            scope = (q.get("scope", ["all"])[0] or "all")
            agent = q.get("agent", [None])[0]
            sub_only = q.get("all", ["0"])[0] not in ("1", "true", "yes", "on")
            try:
                import sys as _sys
                if OPSDIR not in _sys.path:
                    _sys.path.insert(0, OPSDIR)
                import perf
                data = perf.aggregate(frm=frm, to=to, scope=scope, agent=agent, subagents_only=sub_only)
                body = json.dumps(data).encode()
            except Exception as e:
                body = json.dumps({"error": str(e)}).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store"); self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/perf"):
            body = PERF_PAGE.replace("</body>", TICKET_MODAL + "</body>").encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/flow.json"):
            try:
                body = json.dumps(flow_panel_cached(SCRATCH)).encode(); self.send_response(200)
            except Exception as e:
                body = json.dumps({"error": str(e)}).encode(); self.send_response(500)
            self.send_header("Content-Type", "application/json"); self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Cache-Control", "no-store"); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/flow"):
            body = FLOW_PAGE.encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body); return
        if self.path.startswith("/data.json"):
            try:
                body = json.dumps(gather_cached()).encode()
                self.send_response(200); self.send_header("Content-Type", "application/json")
            except Exception as e:
                body = json.dumps({"error": str(e)}).encode(); self.send_response(500); self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*"); self.send_header("Content-Length", str(len(body)))
            self.end_headers(); self.wfile.write(body)
        else:
            body = PAGE.replace("</body>", TICKET_MODAL + "</body>").encode()
            self.send_response(200); self.send_header("Content-Type", "text/html; charset=utf-8"); self.send_header("Cache-Control", "no-store, must-revalidate")
            self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)

#: The dashboard re-executes itself in place above this RSS (MB): the watchdog's ceiling is 1536,
#: and on 2026-09-23/24 it crossed it twice in one evening (1746 MB after 6.5 h, then 1818 MB 50
#: min after a restart) and was restarted by hand both times - its rebuilds allocate big transient
#: structures and CPython does not hand the freed heap back. Twice by hand -> a rule.
RSS_MAX_MB = int(os.environ.get("MONITOR_RSS_MAX") or 1200)


def _rss_mb():
    try:
        return int(subprocess.run(["ps", "-o", "rss=", "-p", str(os.getpid())], capture_output=True,
                                  text=True, timeout=10).stdout.strip() or 0) / 1024
    except Exception:
        return 0.0


def _rss_guard(limit_mb=None, every_s=60, rss=_rss_mb, execv=os.execv, sleep=time.sleep):
    """Checks own RSS every `every_s`; above the limit, logs and re-executes this script in place
    (same pid and port, ~2 s without a dashboard). Returns only in tests (execv stubbed)."""
    limit_mb = limit_mb or RSS_MAX_MB
    while True:
        sleep(every_s)
        mb = rss()
        if mb > limit_mb:
            import sys as _sys
            print(f"monitor: RSS {mb:.0f} MB > {limit_mb} MB - re-executing in place", file=_sys.stderr, flush=True)
            return execv(_sys.executable, [_sys.executable] + _sys.argv)


if __name__ == "__main__":
    import sys
    import launchpath
    launchpath.check(__file__, lambda m: print(f"monitor: {m}", file=sys.stderr, flush=True))
    threading.Thread(target=_rss_guard, daemon=True).start()
    if psutil is not None:
        threading.Thread(target=_cpu_sampler, daemon=True).start()
    threading.Thread(target=_usage_poller, daemon=True).start()
    def _warm_burndown():
        try:
            import burndown
            burndown.series(REPO, os.path.join(SCRATCH, "burndown-cache.json"))
        except Exception:
            pass
    threading.Thread(target=_warm_burndown, daemon=True).start()
    # MONITOR_PORT was documented (ops/README.md, /dev-env) but never read: a second copy always
    # died binding 8901. 8901 stays the default.
    ThreadingHTTPServer(("127.0.0.1", int(os.environ.get("MONITOR_PORT") or 8901)), H).serve_forever()
