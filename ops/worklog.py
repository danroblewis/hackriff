"""Role work log: what each role session SAID at the end of every turn (user, 2026-09-23).

WHY. The user judges the pipeline by reading, not by running commands: "he wants to read YOUR
WORK LOG in the agent dashboard". Every role session's final assistant message of a turn is the
report it wrote for him, and every pipeline-manager tick ends in one `flow:` line (invariant 23).
Both are already in the Claude transcripts; this module finds the role sessions, pulls those
messages out, and ops/monitor.py serves them at /worklog (page) and /worklog.json.

Which session is which role. `~/.claude/sessions/<pid>.json` is Claude Code's registry of LIVE
sessions (`sessionId`, `tmux` = "flow:@106.%106"); a session's role comes from its process argv
(`--append-system-prompt-file .../roles/<role>.md`, how ops/launch.sh starts one) or its tmux
session name (flow / dev / super). What was learned is kept in `$HACKRIFF_OPS/role-sessions.json`
so a session's log stays readable after it exits. A role session that is neither (the supervisor
running in the user's own terminal) is registered by hand in that file:
    {"<session id>": {"role": "supervisor", "source": "manual"}}
`$HACKRIFF_OPS/coordinator-session` (the dashboard's existing pointer) seeds the coordinator.

A turn is everything after one real user message (typed, relayed, or a task notification) up to
the next; its final message is the last contiguous run of assistant text blocks in it (a tool
call after text resets the run, so narration mid-turn is not reported as the result). Sidechain
(subagent) records and tool results are not turns. Transcripts are 30-100 MB, so each file is
parsed once and then incrementally from the byte offset it had reached.
"""
from __future__ import annotations

import glob
import json
import os
import re
import subprocess
import threading
import time

PROJ = os.path.expanduser("~/.claude/projects/-Users-daniellewis-hackriff")
SESSIONS = os.path.expanduser("~/.claude/sessions")
OPS = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
TMUX_ROLE = {"flow": "pipeline-manager", "dev": "coordinator", "super": "supervisor"}
ROLE_ORDER = ["pipeline-manager", "coordinator", "supervisor"]
_ROLE_ARG = re.compile(r"roles/([\w-]+)\.md")
_TURN_LINE = re.compile(r'"type":\s*"(user|assistant)"')


def _argv(pid: int) -> str:
    try:
        return subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True,
                              text=True, timeout=5).stdout.strip()
    except Exception:
        return ""


def discover(sessions_dir: str = SESSIONS, ops: str = OPS, now: float | None = None, argv=_argv) -> dict:
    """{session id: {role, source, first_seen, last_seen, pid, live}} - the registry, updated."""
    now = now or time.time()
    path = os.path.join(ops, "role-sessions.json")
    try:
        reg = json.load(open(path))
        if not isinstance(reg, dict):
            reg = {}
    except Exception:
        reg = {}
    for rec in reg.values():
        rec["live"] = False
    try:
        sid = open(os.path.join(ops, "coordinator-session")).read().strip()
        if sid and sid not in reg:
            reg[sid] = {"role": "coordinator", "source": "coordinator-session", "first_seen": now}
    except OSError:
        pass
    for f in glob.glob(os.path.join(sessions_dir, "*.json")):
        try:
            d = json.load(open(f))
            sid, pid = d["sessionId"], int(d["pid"])
        except Exception:
            continue
        m = _ROLE_ARG.search(argv(pid))
        role, source = (m.group(1), "argv") if m else (None, "")
        if not role and d.get("tmux"):
            role = TMUX_ROLE.get(str(d["tmux"]).split(":")[0])
            source = "tmux"
        rec = reg.get(sid)
        if not role and not rec:
            continue
        rec = rec or {"first_seen": now}
        if role and rec.get("source") != "manual":
            rec["role"], rec["source"] = role, source
        rec.update(pid=pid, last_seen=now, live=True)
        reg[sid] = rec
    try:                                         # small; rewritten atomically each call
        tmp = path + ".tmp"
        with open(tmp, "w") as fh:
            json.dump({k: {x: y for x, y in v.items() if x != "live"} for k, v in reg.items()}, fh, indent=1)
        os.replace(tmp, path)
    except OSError:
        pass
    return reg


def _text_of(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(x.get("text", "") for x in content if isinstance(x, dict) and x.get("type") == "text")
    return ""


def _is_tool_result(content) -> bool:
    return isinstance(content, list) and any(isinstance(x, dict) and x.get("type") == "tool_result" for x in content)


def _first_line(s: str, n: int = 160) -> str:
    for ln in s.splitlines():
        ln = ln.strip()
        if ln:
            return ln[:n]
    return ""


def _close(cur: dict | None, turns: list) -> None:
    if cur and cur["parts"]:
        turns.append({"t": cur["t"], "prompt": cur["prompt"], "text": "\n\n".join(cur["parts"]).strip()})


def feed(state: dict, lines) -> None:
    """Advance one transcript's parse state over new jsonl lines (pure; the unit tests drive it)."""
    turns, cur = state["turns"], state.get("cur")
    for line in lines:
        if not _TURN_LINE.search(line):          # cheap skip: most lines are tool/progress records
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        if o.get("isSidechain"):
            continue
        msg = o.get("message") or {}
        content = msg.get("content")
        if o.get("type") == "user":
            if o.get("isMeta") or _is_tool_result(content):
                continue
            text = _text_of(content).strip()
            if not text:
                continue
            _close(cur, turns)
            cur = {"t": o.get("timestamp"), "prompt": _first_line(text), "parts": [], "run": False}
        elif o.get("type") == "assistant" and cur is not None and isinstance(content, list):
            for b in content:
                if not isinstance(b, dict):
                    continue
                if b.get("type") == "text" and b.get("text", "").strip():
                    if not cur["run"]:
                        cur["parts"], cur["run"] = [], True
                    cur["parts"].append(b["text"])
                    cur["t"] = o.get("timestamp") or cur["t"]
                elif b.get("type") == "tool_use":
                    cur["run"] = False
    state["cur"] = cur


_CACHE: dict[str, dict] = {}
_LOCK = threading.Lock()


def turns_of(path: str) -> list[dict]:
    """Every turn of one transcript, oldest first; the last may be open (`open: True`)."""
    try:
        size = os.path.getsize(path)
    except OSError:
        return []
    with _LOCK:
        st = _CACHE.get(path)
        if st is None or size < st["off"]:
            st = _CACHE[path] = {"off": 0, "turns": [], "cur": None}
        if size > st["off"]:
            with open(path, "rb") as fh:
                fh.seek(st["off"])
                chunk = fh.read(size - st["off"])
            end = chunk.rfind(b"\n") + 1               # a partly-written last line waits
            feed(st, chunk[:end].decode("utf-8", "replace").splitlines())
            st["off"] += end
        out = list(st["turns"])
        cur = st.get("cur")
        if cur and cur["parts"]:
            out.append({"t": cur["t"], "prompt": cur["prompt"], "text": "\n\n".join(cur["parts"]).strip(), "open": True})
        return out


def build(limit: int = 60, proj: str = PROJ, reg: dict | None = None) -> dict:
    reg = discover() if reg is None else reg
    roles = sorted({v.get("role") for v in reg.values() if v.get("role")},
                   key=lambda r: (ROLE_ORDER.index(r) if r in ROLE_ORDER else 99, r))
    out = []
    for role in roles:
        sess = sorted(((sid, v) for sid, v in reg.items() if v.get("role") == role),
                      key=lambda kv: -(kv[1].get("last_seen") or kv[1].get("first_seen") or 0))
        turns, sessions = [], []
        for sid, v in sess:
            path = os.path.join(proj, sid + ".jsonl")
            if not os.path.exists(path):
                continue
            ts = turns_of(path)
            sessions.append({"id": sid, "live": bool(v.get("live")), "turns": len(ts), "source": v.get("source")})
            for t in ts:
                t = dict(t, session=sid[:8])
                t["flow"] = [ln.strip() for ln in t["text"].splitlines() if ln.strip().startswith("flow:")]
                turns.append(t)
        turns.sort(key=lambda t: t.get("t") or "", reverse=True)
        out.append({"role": role, "sessions": sessions, "turns": turns[:limit]})
    return {"generated": time.strftime("%Y-%m-%d %H:%M:%S"), "roles": out}


PAGE = r"""<!doctype html><html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1"><title>Role work log</title>
<script src="https://cdnjs.cloudflare.com/ajax/libs/marked/12.0.2/marked.min.js"></script>
<style>
:root{--bg:#0D1317;--panel:#131B20;--line:#243039;--txt:#D5DEE2;--mut:#8595A0;--dim:#5A6973;--teal:#52C2AE;--amber:#F0A542;--coral:#E47B68;--mono:"SFMono-Regular",Menlo,monospace}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--txt);font:14px/1.55 -apple-system,system-ui,sans-serif}
.top{display:flex;align-items:center;gap:12px;flex-wrap:wrap;padding:8px 16px;border-bottom:1px solid var(--line);background:var(--panel);position:sticky;top:0;z-index:2}
.top .nm{font-weight:600}.top .nm b{color:var(--amber)}.top a{color:var(--mut);text-decoration:none;font-size:12px}.top a:hover{color:var(--txt)}
.top .sub{color:var(--dim);font:11.5px var(--mono);margin-left:auto}
.wrap{max-width:1000px;margin:0 auto;padding:12px 16px 40px}
.tabs{display:flex;gap:6px;flex-wrap:wrap;margin-bottom:10px}
.tab{border:1px solid var(--line);background:var(--panel);color:var(--mut);border-radius:20px;padding:3px 12px;font-size:12.5px;cursor:pointer}
.tab.on{color:var(--bg);background:var(--amber);border-color:var(--amber)}
.tab .n{opacity:.7;margin-left:4px;font:11px var(--mono)}
.ctl{display:flex;gap:14px;align-items:center;color:var(--mut);font-size:12.5px;margin-bottom:10px;flex-wrap:wrap}
.ctl label{cursor:pointer;display:flex;gap:6px;align-items:center}
.sess{color:var(--dim);font:11px var(--mono)}
.turn{background:var(--panel);border:1px solid var(--line);border-radius:9px;margin-bottom:8px;overflow:hidden}
.turn .hd{display:flex;gap:10px;align-items:baseline;padding:8px 12px;cursor:pointer}
.turn .hd:hover{background:rgba(255,255,255,.02)}
.turn .when{color:var(--dim);font:11.5px var(--mono);white-space:nowrap;flex:0 0 auto}
.turn .first{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.turn.open .first{white-space:normal}
.turn .badge{font:10.5px var(--mono);color:var(--teal);border:1px solid var(--teal);border-radius:10px;padding:0 6px;flex:0 0 auto}
.turn .body{display:none;padding:2px 14px 12px;border-top:1px solid var(--line);overflow-wrap:anywhere}
.turn.open .body{display:block}
.turn .prompt{color:var(--dim);font-size:12px;margin:8px 0 4px}
.flowline{font:12.5px var(--mono);padding:6px 12px;border-bottom:1px solid var(--line);display:flex;gap:10px}
.flowline:last-child{border-bottom:0}
.md h1,.md h2,.md h3,.md h4{font-size:14px;color:var(--amber);margin:12px 0 4px}
.md p{margin:6px 0}.md ul,.md ol{margin:4px 0;padding-left:20px}
.md code{font-family:var(--mono);background:rgba(255,255,255,.06);color:#9fd0c0;padding:1px 4px;border-radius:4px;font-size:12px}
.md pre{background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:8px 10px;overflow:auto;font-size:12px}
.md pre code{background:none;padding:0;color:#b6c6d0}
.md table{border-collapse:collapse;margin:6px 0;display:block;overflow-x:auto;font-size:12.5px}
.md th,.md td{border:1px solid var(--line);padding:3px 8px;text-align:left;vertical-align:top}
.md th{color:var(--mut)}.md a{color:var(--amber)}.md b,.md strong{color:#fff}
.empty{color:var(--dim);padding:20px 0}
</style></head><body>
<div class=top><span class=nm>hack<b>riff</b> · role work log</span><a href="/">← dashboard</a><span class=sub id=sub>loading…</span></div>
<div class=wrap>
<div class=tabs id=tabs></div>
<div class=ctl><label><input type=checkbox id=flowonly> <code>flow:</code> lines only</label><span class=sess id=sess></span></div>
<div id=list></div>
</div>
<script>
const esc=s=>String(s==null?'':s).replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
let D=null, role=null, opened=new Set();
try{ role=localStorage.getItem('wl-role'); document.getElementById('flowonly').checked=localStorage.getItem('wl-flow')==='1'; }catch(e){}
const hasMarked=()=>typeof marked!=='undefined'&&marked.parse;
if(hasMarked()) marked.use({gfm:true,breaks:false,renderer:{html(t){return esc(typeof t==='object'?t.text:t);}}});
const md=s=>hasMarked()?marked.parse(String(s)):'<pre style="white-space:pre-wrap">'+esc(s)+'</pre>';
const mdi=s=>hasMarked()?marked.parseInline(String(s)):esc(s);
const when=t=>{ if(!t) return ''; const d=new Date(t); return d.toLocaleString(undefined,{month:'short',day:'numeric',hour:'2-digit',minute:'2-digit'}); };
function first(s){ for(const l of String(s).split('\n')){ const x=l.trim(); if(x) return x.replace(/^#+\s*/,''); } return ''; }
function render(){
  if(!D) return;
  const roles=D.roles||[]; if(!role||!roles.find(r=>r.role===role)) role=roles.length?roles[0].role:null;
  document.getElementById('tabs').innerHTML=roles.map(r=>`<span class="tab${r.role===role?' on':''}" data-r="${esc(r.role)}">${esc(r.role)}<span class=n>${r.turns.length}</span></span>`).join('')||'<span class=empty>no role sessions found</span>';
  const R=roles.find(r=>r.role===role); const L=document.getElementById('list');
  document.getElementById('sess').textContent=R?R.sessions.map(s=>s.id.slice(0,8)+(s.live?' (live)':'')).join(' · '):'';
  if(!R){ L.innerHTML=''; return; }
  if(document.getElementById('flowonly').checked){
    const rows=[]; R.turns.forEach(t=>(t.flow||[]).forEach(f=>rows.push(`<div class=flowline><span class=when>${esc(when(t.t))}</span><span>${esc(f)}</span></div>`)));
    L.innerHTML=rows.length?`<div class=turn>${rows.join('')}</div>`:'<div class=empty>no flow: lines yet</div>'; return;
  }
  L.innerHTML=R.turns.map(t=>{ const k=t.session+t.t; return `<div class="turn${opened.has(k)?' open':''}" data-k="${esc(k)}"><div class=hd><span class=when>${esc(when(t.t))}</span><span class=first>${mdi(first(t.text))}</span>${t.open?'<span class=badge title="this turn has not ended: its text so far, not its final report">in progress</span>':''}</div><div class="body md"><div class=prompt>↳ ${esc(t.prompt)}</div>${opened.has(k)?md(t.text):''}</div></div>`; }).join('')||'<div class=empty>no turns yet</div>';
}
document.addEventListener('click',e=>{
  const tab=e.target.closest('.tab'); if(tab){ role=tab.dataset.r; try{localStorage.setItem('wl-role',role);}catch(x){} render(); return; }
  if(e.target.closest('a')) return;
  const hd=e.target.closest('.turn .hd'); if(!hd) return; const T=hd.parentElement, k=T.dataset.k;
  if(opened.has(k)) opened.delete(k); else opened.add(k); render();
});
document.getElementById('flowonly').onchange=e=>{ try{localStorage.setItem('wl-flow',e.target.checked?'1':'0');}catch(x){} render(); };
async function tick(){ try{ D=await (await fetch('/worklog.json',{cache:'no-store'})).json(); document.getElementById('sub').textContent='updated '+(D.generated||''); render(); }catch(e){ document.getElementById('sub').textContent='error: '+e; } }
tick(); setInterval(tick,30000);
</script></body></html>
"""
