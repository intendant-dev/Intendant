#!/usr/bin/env python3
"""Opt-in native managed-browser workspace proof. Never uses the installed daemon."""
from __future__ import annotations
import argparse, base64, hashlib, http.client, http.server, importlib.util
import json, os, re, shutil, subprocess, sys, tempfile, threading, time
from pathlib import Path

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
    args = parser.parse_args()
    if not args.allow_shared_session_monitor or not __debug__:
        parser.error('explicit native monitor opt-in and Python assertions are required')
    binary, app = args.bin.resolve(strict=True), args.browser_app.resolve(strict=True)
    if args.report.exists():
        parser.error('report must be a fresh path; never overwrite an earlier attempt')
    args.report.parent.mkdir(parents=True, exist_ok=True)
    monitor_harness = load('workspace_monitor_harness', 'verify-macos-monitor-http.py')
    browser_harness = load('workspace_browser_harness', 'verify-macos-chromium-controls.py')
    root = Path(tempfile.mkdtemp(prefix='intendant-browser-workspace-proof-'))
    os.chmod(root, 0o700)
    report = {'before': monitor_harness.inventory(), 'checks': {}, 'rig': str(root),
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'binary_version': subprocess.check_output([str(binary), '--version'], text=True).strip(),
              'automatic_input_retry': False, 'installed_daemon_changed': False}
    daemon = cdp = monitor = workspace = None
    port = token = None
    seq = 0
    cleaned = False
    page = Path(__file__).resolve().parent.parent / 'tests/fixtures/macos-monitor/browser.html'
    contents = page.read_bytes()
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
        value = cdp.call('Runtime.evaluate', {'expression':'window.fixtureStatus()',
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
        before = fixture_status(); report['before_controls'] = before
        assert before['button_count'] == 0 and before['text'] == 'initial disposable text', before
        binding = workspace['macos_window_binding']
        report["initial_read_attempts"] = []
        elements = read_ready_controls(tool, binding, report["initial_read_attempts"])
        report['elements'] = elements; checkpoint(); assert elements.get('ok') is True, elements
        fields = [e for e in elements['controls'] if e['role']=='AXTextField' and e['label']=='Normal browser field']
        assert len(fields) == 1, elements
        desired = 'Managed virtual-display workspace works'
        _, result = tool('act_macos_window_element', {'binding':binding,'element':fields[0]['element'],
            'action':{'type':'set_value','text':desired}})
        report['text_action'] = result; checkpoint(); assert result.get('ok') is True, result
        observed = fixture_status(); report['after_text'] = observed
        assert observed['text'] == desired and observed['button_count'] == 0, observed
        report["button_read_attempts"] = []
        elements = read_ready_controls(tool, binding, report["button_read_attempts"])
        report['button_elements'] = elements; checkpoint(); assert elements.get('ok') is True, elements
        buttons = [e for e in elements['controls'] if e['role']=='AXButton' and e['label']=='Increment browser counter']
        assert len(buttons) == 1, elements
        _, result = tool('act_macos_window_element', {'binding':binding,'element':buttons[0]['element'],
            'action':{'type':'press'}})
        report['button_action'] = result; checkpoint(); assert result.get('ok') is True, result
        after = fixture_status(); report['after_controls'] = after
        assert after['text'] == desired and after['button_count'] == 1 and after['canvas_count'] == 0, after
        captured, meta = tool('take_screenshot', {'display_target':monitor['display_target'],'ephemeral':True})
        report['capture'] = meta; checkpoint()
        images = [item for item in captured.get('content',[]) if item.get('type')=='image']
        assert meta.get('capture_ready') is True and meta.get('artifact_retained') is False and len(images)==1, meta
        pixels = base64.b64decode(images[0]['data'], validate=True)
        report['capture_png_sha256'] = hashlib.sha256(pixels).hexdigest()
        report['checks']['form_effects_verified'] = True
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
        report['after'] = monitor_harness.inventory()
        report['inventory_restored'] = report['after'] == report['before']
        report['ok'] = report.get('ok') is True and cleaned and report['inventory_restored']
        checkpoint()
        if cleaned: shutil.rmtree(root)
    print(json.dumps(report,indent=2))
    return 0 if report['ok'] else 1

if __name__ == '__main__':
    raise SystemExit(main())
