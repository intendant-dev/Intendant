#!/usr/bin/env python3
"""Two-daemon federation rig for task #38 (peer sessions in the scene).

Spawns peer daemon A (mock provider, git project) and primary daemon B,
federates B->A via /api/peers, delegates a scripted task to A *through*
B, and asserts B's /api/peers carries A's folded sessions:

  - the delegated child (label, phase working -> done, lingers as done —
    the persistent-daemon model keeps sessions after completion),
  - A's own primary session stamped is_primary and carrying git vitals,
  - retirement when the child is stopped on A (stop_session over /ws).

Native daemon-lane children emit no per-session usage/limits (the known
#40 gap) — those legs are covered by unit tests against the wire shapes.

  python3 peer-rig.py            # API-only proof (short sleep step)
  python3 peer-rig.py --browser  # + headless-Chrome Station probe
"""
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import tempfile
import urllib.error
import urllib.request

WORKTREE = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
# CI passes the debug binary via INTENDANT_BIN (the smokes are protocol
# probes — profile doesn't change what they catch); the local post-landing
# battery keeps exercising the release artifact by default.
BIN = os.environ.get("INTENDANT_BIN") or os.path.join(WORKTREE, "target/release/intendant")
SCRATCH = os.environ.get("PEER_RIG_SCRATCH") or tempfile.mkdtemp(prefix="peer-rig-")
# Daemon ports are kernel-assigned (`--web 0`, parsed from the Dashboard
# log line) so concurrent rig runs on one box — two CI runner instances,
# or a rig beside a dev daemon — can't collide or cross-talk. The CDP
# port only exists on the local --browser leg; override it if 9333 is
# taken.
CDP_PORT = int(os.environ.get("PEER_RIG_CDP_PORT", "9333"))
BROWSER = "--browser" in sys.argv
SLEEP_STEP = 30 if BROWSER else 6
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
CDP = os.path.expanduser("~/projects/smoke-agent-unification/cdp.py")

procs = []
T0 = time.time()


def log(msg):
    print(f"[rig +{time.time() - T0:6.1f}s] {msg}", flush=True)


def cleanup():
    for name, p in procs:
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
    deadline = time.time() + 5
    for name, p in procs:
        while p.poll() is None and time.time() < deadline:
            time.sleep(0.1)
        if p.poll() is None:
            p.kill()


def http(method, url, body=None, timeout=5):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode() or "{}")


def ws_action(port, payload):
    """Send one control frame to a daemon's /ws and close."""
    from websockets.sync.client import connect
    with connect(f"ws://127.0.0.1:{port}/ws", close_timeout=2) as ws:
        ws.send(json.dumps(payload))
        time.sleep(0.5)


def wait_for(desc, fn, timeout=45, interval=0.25):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            got = fn()
            if got is not None:
                return got
        except Exception as e:  # noqa: BLE001
            last = e
        time.sleep(interval)
    raise SystemExit(f"TIMEOUT waiting for {desc} (last error: {last})")


def spawn_daemon(name, home, script, cwd):
    os.makedirs(home, exist_ok=True)
    os.makedirs(cwd, exist_ok=True)
    script_path = os.path.join(home, "mock_script.json")
    with open(script_path, "w") as f:
        json.dump(script, f, indent=1)
    env = {k: v for k, v in os.environ.items()
           if k not in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY",
                        "MODEL_NAME", "PRESENCE_PROVIDER", "PRESENCE_MODEL",
                        "CU_PROVIDER", "CU_MODEL")}
    env.update(HOME=home, PROVIDER="mock", INTENDANT_MOCK_SCRIPT=script_path)
    log_path = os.path.join(home, "daemon.log")
    out = open(log_path, "w")
    # `--web 0` asks the kernel for a free port (race-free — the daemon
    # binds it directly, no probe-then-reuse window) and the daemon
    # prints the port it actually bound. A 127.0.0.1 bind self-advertises
    # ws://127.0.0.1:<actual>/ws on the agent card, so no --advertise-url
    # is needed.
    p = subprocess.Popen(
        [BIN, "--web", "0", "--bind", "127.0.0.1", "--no-tls", "--no-tui",
         "--autonomy", "full"],
        cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=out)
    procs.append((name, p))

    def parse_port():
        if p.poll() is not None:
            raise SystemExit(f"{name} exited early (code {p.poll()}) — see {log_path}")
        with open(log_path) as f:
            m = re.search(r"Dashboard: https?://127\.0\.0\.1:(\d+)", f.read())
        return int(m.group(1)) if m else None

    port = wait_for(f"{name} bound port", parse_port, timeout=30)
    log(f"daemon {name} pid {p.pid} port {port} home {home}")
    wait_for(f"{name} agent card",
             lambda: http("GET", f"http://127.0.0.1:{port}/.well-known/agent-card.json"))
    return p, port


PEER_TASK_MARK = "PEER RIG TASK"
script_a = {
    "profiles": [
        {"match": PEER_TASK_MARK, "steps": [
            {"content": "Starting the delegated work.",
             "tool_calls": [{"name": "exec_command",
                             "arguments": {"nonce": 1, "command": "echo PEER_RIG_STEP1"}}]},
            {"content": "Holding the probe window open.",
             "tool_calls": [{"name": "exec_command",
                             "arguments": {"nonce": 2, "command": f"sleep {SLEEP_STEP}"}}]},
            {"expect_transcript_contains": "PEER_RIG_STEP1",
             "content": "Delegated work finished.",
             "tool_calls": [{"name": "signal_done",
                             "arguments": {"message": "peer rig complete"}}]},
        ]},
        {"steps": [
            {"content": "fallback profile (unexpected session)",
             "tool_calls": [{"name": "signal_done",
                             "arguments": {"message": "unexpected session"}}]},
        ]},
    ]
}
script_b = {"profiles": [{"steps": [
    {"content": "primary fallback", "tool_calls": [
        {"name": "signal_done", "arguments": {"message": "unused"}}]}]}]}

failures = []


def check(cond, desc):
    tag = "PASS" if cond else "FAIL"
    log(f"{tag}: {desc}")
    if not cond:
        failures.append(desc)


try:
    for d in ("home-a", "home-b", "proj-a", "proj-b"):
        shutil.rmtree(os.path.join(SCRATCH, d), ignore_errors=True)
    # proj-a is a real dirty git repo so A's primary session grows git
    # vitals (the daemon-side prober), which must ride the peer rail.
    proj_a = os.path.join(SCRATCH, "proj-a")
    os.makedirs(proj_a)
    subprocess.run(["git", "init", "-q"], cwd=proj_a, check=True)
    subprocess.run(["git", "-c", "user.email=rig@local", "-c", "user.name=rig",
                    "commit", "-q", "--allow-empty", "-m", "seed"], cwd=proj_a, check=True)
    with open(os.path.join(proj_a, "dirty.txt"), "w") as f:
        f.write("uncommitted\n")

    _, a_port = spawn_daemon("A(peer)", os.path.join(SCRATCH, "home-a"), script_a, proj_a)
    _, b_port = spawn_daemon("B(primary)", os.path.join(SCRATCH, "home-b"), script_b,
                             os.path.join(SCRATCH, "proj-b"))

    add = http("POST", f"http://127.0.0.1:{b_port}/api/peers",
               {"card_url": f"http://127.0.0.1:{a_port}/.well-known/agent-card.json",
                "label": "rig-peer"})
    log(f"peer add -> {add}")

    def connected():
        for p in http("GET", f"http://127.0.0.1:{b_port}/api/peers")["peers"]:
            if p.get("connection_state", {}).get("state") == "connected":
                return p
        return None

    peer = wait_for("B->A connected", connected)
    peer_id = peer["id"]
    log(f"connected: {peer_id}")

    def peer_sessions():
        for p in http("GET", f"http://127.0.0.1:{b_port}/api/peers")["peers"]:
            if p["id"] == peer_id:
                return p.get("sessions", [])
        return []

    task = http("POST", f"http://127.0.0.1:{b_port}/api/peers/{peer_id}/task",
                {"instructions": f"{PEER_TASK_MARK} - run the scripted steps"})
    log(f"delegated -> {task}")

    def child_of(sessions):
        live = [s for s in sessions if not s.get("is_primary")]
        return live[0] if live else None

    child = wait_for("a folded child session on B",
                     lambda: child_of(peer_sessions()), timeout=60)
    log(f"first folded child: {json.dumps(child)[:400]}")
    check(PEER_TASK_MARK in (child.get("label") or ""),
          f"label carries the instructions (got {child.get('label')!r})")
    check(bool(child.get("started_at")), "started_at present")

    working = wait_for("child phase 'working' (lifecycle fold)",
                       lambda: (lambda c: c if c and c.get("phase") == "working" else None)(
                           child_of(peer_sessions())), timeout=30)
    check(working.get("phase") == "working", "phase folded to 'working' from TurnStarted")
    child_sid = working["session_id"]

    if BROWSER:
        chrome_profile = os.path.join(SCRATCH, "chrome-profile")
        shutil.rmtree(chrome_profile, ignore_errors=True)
        chrome = subprocess.Popen(
            [CHROME, "--headless=new", f"--remote-debugging-port={CDP_PORT}",
             f"--user-data-dir={chrome_profile}", "--no-first-run",
             "--window-size=1600,1000", "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(("chrome", chrome))

        def cdp(mode, arg):
            r = subprocess.run([sys.executable, CDP, mode, arg],
                               capture_output=True, text=True, timeout=60)
            if r.returncode != 0:
                raise RuntimeError(f"cdp {mode} failed: {r.stderr.strip()[:400]}")
            return r.stdout.strip()

        wait_for("chrome CDP", lambda: cdp("eval", "1+1"))
        cdp("nav", f"http://127.0.0.1:{b_port}/")
        wait_for("dashboard booted", lambda: True if cdp(
            "eval", "!!document.querySelector('.tab-btn')") == "true" else None)
        cdp("eval", "(() => { const b = [...document.querySelectorAll('button.tab-btn')]"
                    ".find(x => /station/i.test(x.textContent||'')); if (b) b.click();"
                    " return b ? 'clicked' : 'no-station-tab'; })()")

        def scene_peer_nodes():
            out = cdp("eval",
                      "(() => { try { const s = stationProbe.snapshot();"
                      " const peers = (s.agents||[]).filter(a=>String(a.id).startsWith('peer-'));"
                      " return JSON.stringify(peers.map(a=>({id:a.id, phase:a.phase,"
                      "  task:String(a.task||'').slice(0,40), git:a.vitalsGit||null})));"
                      " } catch (e) { return 'ERR:'+e.message; } })()")
            log(f"scene peer nodes: {out[:400]}")
            return out if "peer-session-" in out else None

        nodes = wait_for("peer-session node in the Station scene",
                         scene_peer_nodes, timeout=40, interval=1.0)
        check("peer-session-" in nodes,
              f"Station scene renders the peer session ({nodes[:200]})")
        cdp("shot", os.path.join(SCRATCH, "station-peer-session.png"))
        log(f"screenshot: {os.path.join(SCRATCH, 'station-peer-session.png')}")

    done = wait_for("child phase 'done' after completion",
                    lambda: (lambda c: c if c and c.get("phase") == "done" else None)(
                        child_of(peer_sessions())), timeout=SLEEP_STEP + 60)
    check(done.get("phase") == "done", "phase folded to 'done' from TaskComplete")

    time.sleep(3)
    linger = child_of(peer_sessions())
    check(linger is not None and linger.get("phase") == "done",
          "completed session lingers as done (persistent-daemon parity)")

    def primary_with_vitals():
        for s in peer_sessions():
            if s.get("is_primary") and (s.get("vitals") or {}).get("git"):
                return s
        return None

    prim = wait_for("A's primary session with git vitals", primary_with_vitals, timeout=90)
    git = prim["vitals"]["git"]
    check(prim.get("is_primary") is True, "primary session stamped is_primary")
    check((git.get("dirtyFiles") or 0) >= 1,
          f"git vitals rode the peer rail (dirtyFiles={git.get('dirtyFiles')})")

    log(f"stopping child {child_sid} on A via /ws stop_session")
    ws_action(a_port, {"action": "stop_session", "session_id": child_sid})
    wait_for("child retirement after stop",
             lambda: True if child_of(peer_sessions()) is None else None, timeout=30)
    check(True, "SessionEnded retired the folded child")

finally:
    cleanup()

print()
if failures:
    print(f"RESULT: FAIL ({len(failures)} failures)")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("RESULT: PASS")
