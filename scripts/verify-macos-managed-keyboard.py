#!/usr/bin/env python3
"""Opt-in managed-browser keyboard proof with independent page and OS evidence. Never uses the installed daemon."""
from __future__ import annotations
import argparse, base64, hashlib, http.client, http.server, importlib.util
import json, os, re, shutil, subprocess, sys, tempfile, threading, time
from pathlib import Path
import select, uuid
from macos_managed_keyboard_evidence import verify_transition, verify_witness

def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module

def read_ready_controls(tool, binding, evidence, pause=time.sleep):
    """At most three fresh read-only snapshots for transient AX startup latency.

    Every failed observation is retained. This helper cannot send or replay input,
    and a successful snapshot must independently pass all existing protection checks.
    """
    for attempt in range(3):
        _, result = tool("read_macos_window_elements", {"binding": binding})
        evidence.append(result)
        if result.get("ok") is True:
            return result
        if "kAXErrorCannotComplete (-25204)" not in result.get("error", ""):
            return result
        if attempt < 2:
            pause(.2)
    return result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', required=True, type=Path)
    parser.add_argument('--browser-app', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--allow-shared-session-monitor', action='store_true')
    parser.add_argument('--plan', required=True, type=Path)
    parser.add_argument('--observer', required=True, type=Path)
    parser.add_argument('--require-activity', action='store_true')
    args = parser.parse_args()
    plan = json.loads(args.plan.read_text())
    if not isinstance(plan, dict) or not isinstance(plan.get('keyboard_tool'), str):
        parser.error('a fixed keyboard tool and step plan are required')
    if plan['keyboard_tool'] != 'execute_browser_workspace_keyboard':
        parser.error('only the explicitly browser-scoped keyboard entry point is accepted')
    if not isinstance(plan.get('steps'), list) or not 1 <= len(plan['steps']) <= 32:
        parser.error('keyboard plan must contain 1..32 explicit steps')
    observer = args.observer.resolve(strict=True)
    witness_process = None

    if sys.platform != 'darwin' or not args.allow_shared_session_monitor or not __debug__:
        parser.error('explicit native monitor opt-in and Python assertions are required')
    binary, app = args.bin.resolve(strict=True), args.browser_app.resolve(strict=True)
    if args.report.exists():
        parser.error('report must be a fresh path; never overwrite an earlier attempt')
    args.report.parent.mkdir(parents=True, exist_ok=True)
    monitor_harness = load('workspace_monitor_harness', 'verify-macos-monitor-http.py')
    browser_harness = load('workspace_browser_harness', 'verify-macos-chromium-controls.py')
    root = Path(tempfile.mkdtemp(prefix='intendant-managed-keyboard-proof-'))
    os.chmod(root, 0o700)
    report = {'before': monitor_harness.inventory(), 'checks': {}, 'rig': str(root),
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'binary_version': subprocess.check_output([str(binary), '--version'], text=True).strip(),
              'automatic_input_retry': False, 'installed_daemon_changed': False, 'observer_clock':'CLOCK_MONOTONIC',
              'harness_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'observer_sha256':hashlib.sha256(observer.read_bytes()).hexdigest(),
              'plan_sha256':hashlib.sha256(args.plan.read_bytes()).hexdigest(),
              'source_head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=Path(__file__).resolve().parent.parent,text=True).strip()}
    daemon = cdp = monitor = workspace = None
    port = token = None
    seq = 0
    cleaned = False
    page = Path(__file__).resolve().parent.parent / 'tests/fixtures/macos-monitor/managed-keyboard.html'
    contents = page.read_bytes()
    report['fixture_sha256'] = hashlib.sha256(contents).hexdigest()
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path != '/fixture':
                self.send_error(404); return
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.send_header('Content-Length', str(len(contents)))
            self.end_headers(); self.wfile.write(contents)
        def log_message(self, *unused):
            pass
    web = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=web.serve_forever, daemon=True); thread.start()
    def checkpoint():
        args.report.write_text(json.dumps(report, indent=2) + '\n')
    def tool(name, arguments):
        nonlocal seq
        seq += 1
        conn = http.client.HTTPConnection('127.0.0.1', port, timeout=80)
        try:
            conn.request('POST', '/mcp', json.dumps({'jsonrpc':'2.0','id':seq,
                'method':'tools/call','params':{'name':name,'arguments':arguments}}),
                {'Content-Type':'application/json','Accept':'application/json',
                 'x-intendant-loopback-token':token})
            response = conn.getresponse(); raw = response.read(12 * 1024 * 1024 + 1)
            assert len(raw) <= 12 * 1024 * 1024, 'response size limit'
            value = json.loads(raw)
            assert response.status == 200 and 'error' not in value, value
            result = value['result']
            text = '\n'.join(item.get('text','') for item in result.get('content',[]) if item.get('type') == 'text')
            try: data = json.loads(text)
            except ValueError: data = {'error':text}
            return result, data
        finally:
            conn.close()
    def fixture_status():
        value = cdp.call('Runtime.evaluate', {'expression':'window.keyboardFixtureStatus()',
            'returnByValue':True}, session=cdp_session)
        assert 'exceptionDetails' not in value, value
        return value['result']['value']
    try:
        home = root / 'home'; home.mkdir()
        project = root / 'project'; project.mkdir()
        (project / 'intendant.toml').write_text('')
        mock = root / 'mock.json'; mock.write_text('{"profiles":[]}')
        # Read-only fixture reference to the exact supplied managed test bundle.
        # No download, system browser, existing profile or installed app update.
        cache = home / 'Library/Caches/intendant/browser-workspaces'; cache.mkdir(parents=True)
        (cache / app.name).symlink_to(app, target_is_directory=True)
        env = {k:v for k,v in os.environ.items() if k in ('PATH','TMPDIR','LANG','LC_ALL','USER','LOGNAME')}
        env.update(HOME=str(home), USERPROFILE=str(home), PROVIDER='mock',
                   INTENDANT_MOCK_SCRIPT=str(mock), INTENDANT_MOCK_DISPLAY='synthetic',
                   INTENDANT_MOCK_MEMORY='nominal')
        logpath = root / 'daemon.log'
        with logpath.open('wb') as log:
            daemon = subprocess.Popen([str(binary),'--web','0','--bind','127.0.0.1',
                '--no-tui','--no-tls','--autonomy','full'],cwd=project,env=env,
                stdin=subprocess.DEVNULL,stdout=log,stderr=log)
        report['daemon_pid'] = daemon.pid; checkpoint()
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            text = logpath.read_text(errors='replace')
            match = re.search(r'Dashboard:.*?https?://127\.0\.0\.1:(\d+)',text)
            if match:
                port = int(match[1]); tokenpath = home / '.intendant/loopback-tokens' / f'{port}.token'
                if tokenpath.exists(): token = tokenpath.read_text().strip(); break
            if daemon.poll() is not None: raise RuntimeError('isolated daemon exited: '+text[-2000:])
            time.sleep(.1)
        assert port and token, 'isolated daemon did not become ready'
        _, initial = tool('list_macos_monitors', {}); assert initial['monitors'] == [], initial
        _, monitor = tool('create_virtual_display', {'width':800,'height':600})
        report['monitor'] = monitor; checkpoint(); assert monitor.get('ok') is True, monitor
        _, workspace = tool('create_browser_workspace', {'url':f'http://127.0.0.1:{web.server_port}/fixture',
            'label':'managed virtual workspace acceptance','display_target':monitor['display_target']})
        report['workspace'] = workspace; checkpoint()
        assert workspace.get('status') == 'ready' and workspace.get('macos_window_binding'), workspace
        assert workspace['display_target'] == monitor['display_target'], workspace
        report["checks"]["launch_and_placement_verified"] = True
        captured, initial_capture = tool("take_screenshot", {"display_target":monitor["display_target"], "ephemeral":True})
        report["initial_capture"] = initial_capture; checkpoint()
        initial_images = [item for item in captured.get("content", []) if item.get("type") == "image"]
        assert initial_capture.get("capture_ready") is True and initial_capture.get("artifact_retained") is False and len(initial_images) == 1, initial_capture
        initial_png = base64.b64decode(initial_images[0]["data"], validate=True)
        assert initial_png[:8] == bytes([137,80,78,71,13,10,26,10]), "invalid PNG signature"
        assert int.from_bytes(initial_png[16:20], "big") == 800 and int.from_bytes(initial_png[20:24], "big") == 600, "wrong display image dimensions"
        report["checks"]["exact_monitor_capture_verified"] = True
        report["initial_capture_png_sha256"] = hashlib.sha256(initial_png).hexdigest()
        profile = Path(workspace['profile_dir'])
        lines = (profile / 'DevToolsActivePort').read_text().splitlines()
        assert int(lines[0]) == workspace['debugging_port']
        cdp = browser_harness.CDP(int(lines[0]), lines[1])
        processes = cdp.call('SystemInfo.getProcessInfo')['processInfo']
        assert any(row['type']=='browser' and row['id']==workspace['process_id'] for row in processes), processes
        cdp_session = cdp.call('Target.attachToTarget', {'targetId':workspace['active_target_id'],
            'flatten':True})['sessionId']
        before = fixture_status(); report['before_keyboard'] = before
        assert before['first'] == 'seed' and before['second'] == '' and before['events'] == [], before
        binding = workspace['macos_window_binding']
        # The fixture declares page-local autofocus. Do not bootstrap keyboard
        # readiness with an OS mouse pair: that would mix input mechanisms and
        # can activate a native window. No click, focus restoration or fallback.
        assert before['active'] == 'first', 'fixture page-local autofocus unavailable'
        report['setup_native_clicks'] = 0
        witness_process = subprocess.Popen([str(observer),'--observe-readonly-120s'],
            stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        assert select.select([witness_process.stdout],[],[],5)[0], 'observer did not become ready'
        assert json.loads(witness_process.stdout.readline()) == {'ready':True}
        time.sleep(.1)
        report['keyboard_steps'] = []
        replacements = {'$workspace':workspace['id'], '$binding':binding,
                        '$display':monitor['display_target'], '$generation':monitor['capture_generation']}
        def expand(value):
            if isinstance(value,str): return replacements.get(value,value)
            if isinstance(value,list): return [expand(v) for v in value]
            if isinstance(value,dict): return {k:expand(v) for k,v in value.items()}
            return value
        started_us = time.clock_gettime_ns(time.CLOCK_MONOTONIC)//1000
        for index,step in enumerate(plan['steps']):
            arguments = expand(step['arguments'])
            assert arguments.get('workspace_id') == workspace['id'], 'step must name this exact workspace'
            assert arguments.get('display_target',monitor['display_target']) == monitor['display_target']
            prior = fixture_status()
            evidence = {'index':index,'before':prior,'request':arguments,
                        'started_us':time.clock_gettime_ns(time.CLOCK_MONOTONIC)//1000}
            report['keyboard_steps'].append(evidence); checkpoint()
            if step.get('facade'):
                _, reply = tool('act', {'argv':['browser','keyboard',workspace['id'],
                    arguments['request_id'],json.dumps(arguments['action'],ensure_ascii=False)]})
            else:
                _, reply = tool(plan['keyboard_tool'], arguments)
            evidence['finished_us'] = time.clock_gettime_ns(time.CLOCK_MONOTONIC)//1000
            evidence['reply'] = reply
            observed = fixture_status(); evidence['after'] = observed; checkpoint()
            if step.get('expect_refusal'):
                assert reply.get('ok') is not True and step['expect_refusal'] in json.dumps(reply), reply
                for key in ('first','second','editor','active','activations','protectedEvents','events'):
                    assert prior[key] == observed[key], 'refused input changed fixture: '+key
                evidence['refusal_no_effect_verified'] = True
            else:
                assert reply.get('ok') is True and reply.get('error') is None, reply
                evidence['verification'] = verify_transition(prior,observed,step.get('expected',{}),
                    key=step.get('key'),receiver=step.get('receiver'),
                    minimum_input_events=step.get('minimum_input_events',0))
            checkpoint(); time.sleep(.1)
        ended_us = time.clock_gettime_ns(time.CLOCK_MONOTONIC)//1000
        time.sleep(.1)
        output, stderr = witness_process.communicate(b'q',timeout=4)
        report['observer_exit'] = witness_process.returncode
        assert witness_process.returncode == 0, stderr.decode(errors='replace')
        report['observer'] = json.loads(output)
        witness_process = None
        report['witness_verification'] = verify_witness(report['observer'],workspace['process_id'],started_us,ended_us)
        if args.require_activity:
            assert report['witness_verification']['keyboard_activity_observed_in_action_span'], 'no observed concurrent keyboard activity'
        verifications=[step['verification'] for step in report['keyboard_steps'] if 'verification' in step]
        assert any(v['page_key_pairs'] for v in verifications), 'no positive page key-pair evidence'
        assert any(v['input_events'] for v in verifications), 'no positive page text-input evidence'
        report['checks']['keyboard_effects_verified'] = True
        checkpoint()
        captured, meta = tool('take_screenshot', {'display_target':monitor['display_target'],'ephemeral':True})
        report['capture'] = meta; checkpoint()
        images = [item for item in captured.get('content',[]) if item.get('type')=='image']
        assert meta.get('capture_ready') is True and meta.get('artifact_retained') is False and len(images)==1, meta
        pixels = base64.b64decode(images[0]['data'], validate=True)
        report['capture_png_sha256'] = hashlib.sha256(pixels).hexdigest()
        cdp.close(); cdp = None
        _, destroyed = tool('destroy_virtual_display', {'display_id':monitor['display_id'],
            'capture_generation':monitor['capture_generation']})
        report['destroy'] = destroyed; checkpoint()
        assert destroyed.get('ok') is True and workspace['id'] in destroyed['closed_browser_workspace_ids'], destroyed
        assert not profile.exists(), 'private browser profile remains after cleanup'
        _, remaining = tool('list_browser_workspaces', {}); assert remaining == [], remaining
        _, inventory = tool('list_macos_monitors', {}); assert inventory['monitors'] == [], inventory
        monitor = None; cleaned = True
        report['checks']['monitor_destruction_closes_browser'] = True
        report['ok'] = True
    except Exception as error:
        report['ok'] = False; report['error'] = str(error)
    finally:
        if witness_process is not None:
            try:
                output, stderr = witness_process.communicate(b'q',timeout=4)
                report['observer'] = json.loads(output)
                report['observer_exit'] = witness_process.returncode
            except Exception as error:
                report['observer_cleanup_error'] = str(error)
                witness_process.kill(); witness_process.wait(timeout=3)
        if cdp is not None: cdp.close()
        if port and token and monitor and monitor.get('capture_generation'):
            try:
                _, cleanup = tool('destroy_virtual_display', {'display_id':monitor['display_id'],
                    'capture_generation':monitor['capture_generation']})
                report['cleanup'] = cleanup; cleaned = cleanup.get('ok') is True
            except Exception as error: report['cleanup_error'] = str(error)
        elif not monitor:
            cleaned = True
        web.shutdown(); web.server_close(); thread.join(timeout=3)
        if cleaned and daemon is not None:
            monitor_harness.terminate(daemon)
            report['daemon_reaped'] = daemon.poll() is not None
        else:
            report['daemon_retained_for_cleanup'] = True
        # CGVirtualDisplay teardown is asynchronous at the WindowServer edge.
        # The production destroy receipt has already confirmed owner cleanup;
        # wait only for the read-only OS inventory to converge, exactly as the
        # canonical monitor harness does. Never retry destroy or any input.
        inventory_deadline = time.monotonic() + 10
        while True:
            report['after'] = monitor_harness.inventory()
            if report['after'] == report['before'] or time.monotonic() >= inventory_deadline:
                break
            time.sleep(.1)
        report['inventory_restored'] = report['after'] == report['before']
        report['ok'] = report.get('ok') is True and cleaned and report['inventory_restored']
        checkpoint()
        if cleaned: shutil.rmtree(root)
    print(json.dumps(report,indent=2))
    return 0 if report['ok'] else 1

if __name__ == '__main__':
    raise SystemExit(main())
