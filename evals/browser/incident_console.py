#!/usr/bin/env python3
"""Deterministic Incident Console task server and BrowserGym-style task.

The task is intentionally self-contained: CI can exercise setup, reset, and
grading with Python's standard library. BrowserGym/Playwright callers can use
IncidentConsoleTask directly because it implements setup(page) and
validate(page, chat_messages).
"""

from __future__ import annotations

from dataclasses import dataclass
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
from typing import Any
from urllib.parse import parse_qs, urlparse


SEEDS: tuple[dict[str, Any], ...] = (
    {
        "incident": "INC-204",
        "region": "us-west-2",
        "impacted": "checkout-api",
        "source": "pricing-cache",
        "policy": "exponential",
        "nodes": ["edge", "checkout-api", "pricing-cache", "ledger"],
    },
    {
        "incident": "INC-317",
        "region": "eu-central-1",
        "impacted": "billing-api",
        "source": "tax-engine",
        "policy": "linear",
        "nodes": ["tax-engine", "billing-api", "events", "gateway"],
    },
    {
        "incident": "INC-422",
        "region": "ap-southeast-1",
        "impacted": "orders-api",
        "source": "inventory-read",
        "policy": "exponential",
        "nodes": ["orders-api", "inventory-read", "fulfillment", "edge"],
    },
    {
        "incident": "INC-509",
        "region": "us-east-1",
        "impacted": "auth-api",
        "source": "token-store",
        "policy": "linear",
        "nodes": ["gateway", "token-store", "auth-api", "audit"],
    },
)


@dataclass
class TaskState:
    seed: int = 0
    mode: str = "visual"
    selected_incident: str | None = None
    selected_service: str | None = None
    retry_policy: str | None = None
    filter_text: str = ""
    submitted: bool = False
    wrong_mutations: int = 0
    observable: dict[str, Any] | None = None

    @property
    def config(self) -> dict[str, Any]:
        return {**SEEDS[self.seed], "mode": self.mode}

    def observable_state(self) -> dict[str, Any]:
        return self.observable or {
            "overlay": True,
            "filter": "",
            "view": "list",
            "incident": None,
            "service": "",
            "policy": "",
            "result": "",
        }

    def grade(self) -> dict[str, Any]:
        cfg = self.config
        exact = (
            self.submitted
            and self.selected_incident == cfg["incident"]
            and self.selected_service == cfg["source"]
            and self.retry_policy == cfg["policy"]
            and self.filter_text.strip().lower() == cfg["incident"].lower()
            and self.wrong_mutations == 0
        )
        return {
            "success": exact,
            "selectedIncident": self.selected_incident,
            "selectedService": self.selected_service,
            "retryPolicy": self.retry_policy,
            "filterText": self.filter_text,
            "submitted": self.submitted,
            "wrongMutations": self.wrong_mutations,
            "expected": {
                "incident": cfg["incident"],
                "source": cfg["source"],
                "policy": cfg["policy"],
                "filter": cfg["incident"],
            },
        }


class IncidentConsoleServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int]):
        super().__init__(address, IncidentConsoleHandler)
        self.task_state = TaskState()

    @property
    def url(self) -> str:
        host, port = self.server_address
        return f"http://{host}:{port}"


class IncidentConsoleHandler(BaseHTTPRequestHandler):
    server: IncidentConsoleServer

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _json(self, body: dict[str, Any], status: HTTPStatus = HTTPStatus.OK) -> None:
        data = json.dumps(body, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _read_json(self) -> dict[str, Any]:
        length = int(self.headers.get("content-length", "0"))
        return json.loads(self.rfile.read(length) or b"{}")

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        path = urlparse(self.path).path
        if path == "/":
            data = PAGE_HTML.encode()
            self.send_response(HTTPStatus.OK)
            self.send_header("content-type", "text/html; charset=utf-8")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        elif path == "/api/config":
            self._json(self.server.task_state.config)
        elif path == "/api/grade":
            self._json(self.server.task_state.grade())
        elif path == "/api/observable":
            self._json(self.server.task_state.observable_state())
        else:
            self._json({"error": "not found"}, HTTPStatus.NOT_FOUND)

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        path = urlparse(self.path).path
        body = self._read_json()
        state = self.server.task_state
        if path == "/api/reset":
            seed = int(body.get("seed", 0))
            mode = str(body.get("mode", "visual"))
            if not 0 <= seed < len(SEEDS):
                self._json({"error": "seed out of range"}, HTTPStatus.BAD_REQUEST)
                return
            if mode not in {"dom", "visual"}:
                self._json({"error": "mode must be dom or visual"}, HTTPStatus.BAD_REQUEST)
                return
            self.server.task_state = TaskState(seed=seed, mode=mode)
            self._json({"ok": True, "seed": seed, "mode": mode})
        elif path == "/api/select":
            state.selected_incident = str(body.get("incident", ""))
            self._json({"ok": True})
        elif path == "/api/submit":
            state.selected_service = str(body.get("service", ""))
            state.retry_policy = str(body.get("policy", ""))
            cfg = state.config
            correct = (
                state.selected_incident == cfg["incident"]
                and state.selected_service == cfg["source"]
                and state.retry_policy == cfg["policy"]
            )
            if correct:
                state.submitted = True
            else:
                state.wrong_mutations += 1
            self._json({"ok": correct, "grade": state.grade()})
        elif path == "/api/observe":
            allowed = {"overlay", "filter", "view", "incident", "service", "policy", "result"}
            state.observable = {key: body.get(key) for key in allowed}
            state.filter_text = str(body.get("filter", ""))
            self._json({"ok": True})
        else:
            self._json({"error": "not found"}, HTTPStatus.NOT_FOUND)


def start_server() -> tuple[IncidentConsoleServer, threading.Thread]:
    server = IncidentConsoleServer(("127.0.0.1", 0))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread


class IncidentConsoleTask:
    """BrowserGym-compatible task contract backed by the deterministic server."""

    def __init__(self, base_url: str, seed: int = 0, mode: str = "visual"):
        self.base_url = base_url.rstrip("/")
        self.seed = seed
        self.mode = mode

    def setup(self, page: Any) -> tuple[str, dict[str, Any]]:
        page.request.post(
            f"{self.base_url}/api/reset", data={"seed": self.seed, "mode": self.mode}
        )
        page.goto(self.base_url)
        cfg = SEEDS[self.seed]
        return task_prompt(self.seed, evidence=False, mode=self.mode), {"config": cfg}

    def validate(
        self, page: Any, _chat_messages: list[dict[str, Any]]
    ) -> tuple[float, bool, str, dict[str, Any]]:
        grade = page.evaluate("() => fetch('/api/grade').then(r => r.json())")
        success = bool(grade["success"])
        return (1.0 if success else 0.0, True, "exact state" if success else "state mismatch", grade)


def task_prompt(seed: int, evidence: bool, mode: str = "visual") -> str:
    cfg = SEEDS[seed]
    evidence_line = (
        "After the success confirmation appears, share exactly one final screenshot as evidence."
        if evidence
        else "Do not share screenshots or recordings."
    )
    decision = (
        f"From the incident event log, choose the service that sends requests directly "
        f"to {cfg['impacted']}."
        if mode == "dom"
        else f"In the topology, choose the service whose arrow flows directly into the "
        f"highlighted {cfg['impacted']} node."
    )
    return (
        f"Open the Incident Console. Use the incident filter to narrow the table to "
        f"{cfg['incident']}, confirm it is in {cfg['region']}, and investigate it. "
        f"{decision} Apply the {cfg['policy']} retry policy, submit the remediation, "
        f"and verify the success confirmation. {evidence_line} Use only the provided browser CLI "
        "for page interaction; do not call the task server APIs directly or inspect its source."
    )


PAGE_HTML = r'''<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Incident Console</title>
<style>
body{font:15px system-ui;margin:0;background:#f4f6fa;color:#182033}.top{background:#14213d;color:white;padding:18px 28px}
main{max-width:1040px;margin:24px auto;background:white;border-radius:12px;padding:24px;box-shadow:0 8px 30px #16213d18}
input,select,button{font:inherit;padding:9px 12px}button{cursor:pointer;background:#2457d6;color:white;border:0;border-radius:6px}
table{width:100%;border-collapse:collapse;margin-top:16px}th,td{text-align:left;padding:12px;border-bottom:1px solid #dfe4ef}
.grid{display:grid;grid-template-columns:1.4fr 1fr;gap:24px}.panel{border:1px solid #dfe4ef;border-radius:8px;padding:16px}
canvas{width:100%;height:330px;background:#f8fafc;border-radius:8px}.field{display:grid;gap:6px;margin:14px 0}
.overlay{position:fixed;inset:0;background:#0c132880;display:grid;place-items:center;z-index:10}.modal{background:white;padding:28px;border-radius:12px;max-width:420px}
.success{padding:14px;background:#d9f8e5;color:#105c2d;border-radius:8px;margin-top:16px}.error{color:#a31d2b;margin-top:12px}
</style></head><body><div class="top"><strong>Engrams Incident Console</strong></div><main>
<section id="list"><h1>Active incidents</h1><label>Filter incidents <input id="filter" placeholder="incident id or region"></label><div id="table"></div></section>
<section id="detail" hidden><button id="back">Back</button><h1 id="heading"></h1><div class="grid"><div class="panel"><h2>Service topology</h2><p>Arrows show request flow. The impacted service is red.</p><p id="semantic-edge" hidden></p><canvas id="topology" width="620" height="330" aria-label="Service topology diagram"></canvas></div><form class="panel" id="form"><h2>Remediation</h2><div class="field"><label for="service">Direct upstream service</label><select id="service" required></select></div><div class="field"><label for="policy">Retry policy</label><select id="policy" required><option value="">Choose…</option><option value="linear">Linear</option><option value="exponential">Exponential</option></select></div><button type="submit">Apply remediation</button><div id="result" role="status"></div></form></div></section>
</main><div class="overlay" id="overlay"><div class="modal"><h2>Incident data acknowledgement</h2><p>This console contains production-like operational data. Acknowledge before continuing.</p><button id="ack">Acknowledge and continue</button></div></div>
<script>
let cfg;const decoys=[['INC-101','us-east-2'],['INC-278','eu-west-1'],['INC-633','ap-northeast-1']];
function observe(){const result=document.querySelector('#result');fetch('/api/observe',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({overlay:!!document.querySelector('#overlay'),filter:document.querySelector('#filter').value,view:document.querySelector('#detail').hidden?'list':'detail',incident:document.querySelector('#detail').hidden?null:cfg.incident,service:document.querySelector('#service').value,policy:document.querySelector('#policy').value,result:result.textContent})});}
async function boot(){cfg=await fetch('/api/config').then(r=>r.json());renderTable('');document.querySelector('#ack').onclick=()=>{document.querySelector('#overlay').remove();observe();};document.querySelector('#filter').oninput=e=>{renderTable(e.target.value);observe();};document.querySelector('#service').onchange=observe;document.querySelector('#policy').onchange=observe;document.querySelector('#back').onclick=()=>location.reload();observe();}
function renderTable(q){const rows=[[cfg.incident,cfg.region],...decoys].filter(r=>r.join(' ').toLowerCase().includes(q.toLowerCase()));document.querySelector('#table').innerHTML=`<table><thead><tr><th>Incident</th><th>Region</th><th>Status</th><th></th></tr></thead><tbody>${rows.map(r=>`<tr><td>${r[0]}</td><td>${r[1]}</td><td>Investigating</td><td><button data-id="${r[0]}">Investigate</button></td></tr>`).join('')}</tbody></table>`;document.querySelectorAll('[data-id]').forEach(b=>b.onclick=()=>openIncident(b.dataset.id));}
async function openIncident(id){await fetch('/api/select',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({incident:id})});if(id!==cfg.incident)return;document.querySelector('#list').hidden=true;document.querySelector('#detail').hidden=false;document.querySelector('#heading').textContent=`${cfg.incident} · ${cfg.region}`;const edge=document.querySelector('#semantic-edge');edge.hidden=cfg.mode!=='dom';edge.textContent=`Event log: ${cfg.source} sends requests directly to ${cfg.impacted}.`;document.querySelector('#service').innerHTML='<option value="">Choose…</option>'+cfg.nodes.map(n=>`<option value="${n}">${n}</option>`).join('');requestAnimationFrame(draw);observe();}
function draw(){const c=document.querySelector('#topology'),x=c.getContext('2d');x.clearRect(0,0,c.width,c.height);const other=cfg.nodes.filter(n=>n!==cfg.source&&n!==cfg.impacted),corners=[[95,70],[525,70],[525,260],[95,260]],rotations=[[0,2,1,3],[1,3,0,2],[2,0,3,1],[3,1,2,0]],layout=rotations[cfg.incident.charCodeAt(4)%4],pos={};[cfg.impacted,cfg.source,other[0],other[1]].forEach((n,i)=>pos[n]=corners[layout[i]]);function arrow(a,b){const [x1,y1]=pos[a],[x2,y2]=pos[b],dx=x2-x1,dy=y2-y1,len=Math.hypot(dx,dy),sx=x1+70*dx/len,sy=y1+70*dy/len,ex=x2-70*dx/len,ey=y2-70*dy/len,ang=Math.atan2(ey-sy,ex-sx);x.strokeStyle='#53627a';x.lineWidth=3;x.beginPath();x.moveTo(sx,sy);x.lineTo(ex,ey);x.stroke();x.fillStyle='#53627a';x.beginPath();x.moveTo(ex,ey);x.lineTo(ex-15*Math.cos(ang-.45),ey-15*Math.sin(ang-.45));x.lineTo(ex-15*Math.cos(ang+.45),ey-15*Math.sin(ang+.45));x.fill();}arrow(cfg.source,cfg.impacted);arrow(other[0],cfg.source);arrow(cfg.impacted,other[1]);cfg.nodes.forEach(n=>{const [px,py]=pos[n];x.fillStyle=n===cfg.impacted?'#d9485f':'#2457d6';x.beginPath();x.roundRect(px-65,py-23,130,46,9);x.fill();x.fillStyle='white';x.textAlign='center';x.textBaseline='middle';x.font='14px system-ui';x.fillText(n,px,py);});}
document.querySelector('#form').onsubmit=async e=>{e.preventDefault();const service=document.querySelector('#service').value,policy=document.querySelector('#policy').value;const r=await fetch('/api/submit',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({service,policy})}).then(r=>r.json());document.querySelector('#result').className=r.ok?'success':'error';document.querySelector('#result').textContent=r.ok?'Remediation applied successfully':'That remediation does not match the incident topology.';observe();};boot();
</script></body></html>'''


if __name__ == "__main__":
    srv, _ = start_server()
    print(srv.url, flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        srv.shutdown()
