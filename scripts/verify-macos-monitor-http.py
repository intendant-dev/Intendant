#!/usr/bin/env python3
"""Manual native proof: isolated HTTP daemon and test-owned virtual monitor only."""
from __future__ import annotations
import argparse, base64, contextlib, ctypes, http.client, io, json, os, re, subprocess, tempfile, time
from pathlib import Path
from PIL import Image


def inventory():
    cg = ctypes.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
    ids = (ctypes.c_uint32 * 32)(); count = ctypes.c_uint32()
    result = cg.CGGetOnlineDisplayList(32, ids, ctypes.byref(count))
    if result or count.value >= 32:
        raise RuntimeError('Cannot establish complete display inventory')
    return {'primary': cg.CGMainDisplayID(), 'ids': list(ids)[:count.value]}


def terminate(child):
    if child is not None:
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=8)
        except subprocess.TimeoutExpired:
            child.kill(); child.wait(timeout=8)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', required=True)
    parser.add_argument('--fixture', required=True)
    parser.add_argument('--report', required=True)
    parser.add_argument('--allow-shared-session-monitor', action='store_true')
    parser.add_argument('--check-recovery', action='store_true', help='Verify owner monitor inventory and exact read-only status')
    args = parser.parse_args()
    if not args.allow_shared_session_monitor:
        parser.error('Native monitor hotplug requires explicit opt-in')
    if not __debug__:
        parser.error("Run this assertion-based acceptance test without Python -O")
    rig = tempfile.TemporaryDirectory(prefix="intendant-monitor-http-proof-")
    report = {'before': inventory(), 'checks': {}}
    daemon = fixture = None
    active = []
    port = token = None
    seq = 0
    def rpc(method, params, authed=True):
        nonlocal seq
        seq += 1
        headers = {'Content-Type': 'application/json', 'Accept': 'application/json'}
        if authed:
            headers['x-intendant-loopback-token'] = token
        conn = http.client.HTTPConnection('127.0.0.1', port, timeout=25)
        try:
            body = json.dumps({'jsonrpc': '2.0', 'id': seq, 'method': method, 'params': params})
            conn.request('POST', '/mcp', body, headers)
            response = conn.getresponse(); raw = response.read(12 * 1024 * 1024)
            data = json.loads(raw)
            return response.status, data
        finally:
            conn.close()
    def tool(name, arguments):
        status, data = rpc('tools/call', {'name': name, 'arguments': arguments})
        if status != 200 or data.get('error'):
            raise RuntimeError(f'{name}: HTTP {status}: {data}')
        result = data['result']
        text = '\n'.join(c.get('text', '') for c in result.get('content', []) if c.get('type') == 'text')
        try: parsed = json.loads(text)
        except json.JSONDecodeError: parsed = {'text': text}
        return result, parsed
    def recover():
        result, data = tool('list_macos_monitors', {})
        assert not result.get('isError') and isinstance(data.get('monitors'), list), data
        encoded = json.dumps(data)
        assert all(key not in encoded for key in ('"native_id"', '"helper_handle"', '"pid"')), data
        return data['monitors']
    def readonly_status(monitor):
        result, data = tool('display_readiness', {'display_target': monitor['display_target']})
        assert not result.get('isError') and data.get('ready') is False, data
        assert data.get('capture_ready') is False and data.get('input_supported') is False, data
        assert data.get('streaming_supported') is False, data
        assert data.get('target') == monitor['display_target'] and isinstance(data.get('summary'), str), data
        assert len(data.get('layers', [])) == 5, data
        assert not any(c.get('type') == 'image' for c in result.get('content', [])), data
        return data
    def create():
        result, data = tool('create_virtual_display', {'width': 800, 'height': 600})
        assert data.get('ok') is True, data
        assert data['input_supported'] is False and data['streaming_supported'] is False
        active.append(data)
        return data
    def destroy(data):
        _, outcome = tool('destroy_virtual_display', {'display_id': data['display_id'], 'capture_generation': data['capture_generation']})
        assert outcome.get('ok') is True, outcome
        active.remove(data)
    try:
        with contextlib.nullcontext(rig.name) as root:
            root = Path(root); home = root / 'home'; home.mkdir()
            project = root / 'project'; project.mkdir()
            (project / 'intendant.toml').write_text('')
            mock = root / 'mock.json'; mock.write_text('{"profiles":[]}')
            env = {k: v for k, v in os.environ.items() if k in ('PATH', 'TMPDIR', 'LANG', 'LC_ALL', 'USER', 'LOGNAME')}
            env.update(HOME=str(home), USERPROFILE=str(home), PROVIDER='mock', INTENDANT_MOCK_SCRIPT=str(mock),
                       INTENDANT_MOCK_DISPLAY='synthetic', INTENDANT_MOCK_MEMORY='nominal')
            logpath = root / 'daemon.log'
            with logpath.open('wb') as log:
                daemon = subprocess.Popen([args.bin, '--web', '0', '--bind', '127.0.0.1', '--no-tui', '--no-tls', '--autonomy', 'full'],
                                          cwd=project, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            deadline = time.monotonic() + 35
            while time.monotonic() < deadline:
                text = logpath.read_text(errors='replace')
                match = re.search(r'Dashboard:.*?https?://127\.0\.0\.1:(\d+)', text)
                if match:
                    port = int(match[1]); tokenpath = home / '.intendant' / 'loopback-tokens' / f'{port}.token'
                    if tokenpath.exists():
                        token = tokenpath.read_text().strip(); break
                if daemon.poll() is not None: raise RuntimeError('Isolated daemon exited: ' + text[-3000:])
                time.sleep(.15)
            assert port and token, 'Isolated daemon did not become ready'
            assert rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'native-monitor-fixture', 'version': '1'}}, False)[0] == 401
            report['checks']['http_auth_required'] = True
            assert rpc('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {}, 'clientInfo': {'name': 'native-monitor-fixture', 'version': '1'}})[0] == 200
            if args.check_recovery:
                assert recover() == [], 'inventory before creation must be empty'
                assert inventory() == report['before'], 'inspection changed native display inventory'
                report['checks']['empty_inventory_before_create'] = True
            first = create(); report['created'] = first
            if args.check_recovery:
                listed = recover()
                assert len(listed) == 1, listed
                recovered = listed[0]
                assert all(recovered[k] == first[k] for k in ('display_id', 'display_target', 'capture_generation')), listed
                # A client discards the create handle; recovery is a new HTTP call.
                first = recovered
                active[:] = [recovered]
                report['checks']['recovered_generation_handle'] = True
                report['checks']['read_only_status_before_capture'] = readonly_status(first)
            current = inventory(); added = set(current['ids']) - set(report['before']['ids'])
            assert current['primary'] == report['before']['primary'] and len(added) == 1, current
            native_id = added.pop(); assert native_id != current['primary']
            statuspath = root / 'fixture.json'
            fixture = subprocess.Popen([args.fixture, str(native_id), str(statuspath)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            deadline = time.monotonic() + 8
            while not statuspath.exists() and time.monotonic() < deadline and fixture.poll() is None:
                time.sleep(.1)
            assert statuspath.exists(), 'Known-pattern fixture did not open on test monitor'
            report['fixture'] = json.loads(statuspath.read_text())
            time.sleep(.3)
            result, metadata = tool('take_screenshot', {'display_target': first['display_target']})
            images = [c for c in result.get('content', []) if c.get('type') == 'image']
            assert not result.get('isError') and len(images) == 1, metadata
            image = Image.open(io.BytesIO(base64.b64decode(images[0]['data'], validate=True))).convert('RGB')
            assert image.size == (800, 600), image.size
            points = [(200, 150), (600, 150), (200, 450), (600, 450)]
            pixels = [image.getpixel(p) for p in points]
            expected = [(255,0,0),(0,255,0),(0,0,255),(255,255,255)]
            assert all(max(abs(a-b) for a,b in zip(p,e)) < 75 for p,e in zip(pixels,expected)), pixels
            report['checks']['exact_pattern_capture'] = {'size': image.size, 'samples': pixels}
            report['checks']['capture_metadata'] = metadata
            assert Path(metadata['screenshot_path']).stat().st_mode & 0o777 == 0o600
            report['checks']['artifact_mode_0600'] = True
            if args.check_recovery:
                report['checks']['read_only_status_after_capture'] = readonly_status(first)
            for selector in [first['display_target'], ' MACOS_VIRTUAL:bad ', f"display_{first['display_id']}", str(first['display_id'])]:
                denied, detail = tool('execute_cu_actions', {'display_target': selector, 'actions': [{'type':'wait','ms':0}]})
                assert denied.get('isError') is True, (selector, detail)
            report['checks']['input_selectors_refused_without_input'] = True
            terminate(fixture); fixture = None
            destroy(first)
            if args.check_recovery:
                assert recover() == [], 'destroyed generation was still recoverable'
                report['checks']['destroyed_generation_removed'] = True
            second = create()
            denied, detail = tool('take_screenshot', {'display_target': first['display_target']})
            assert denied.get('isError') is True, detail
            _, stale = tool('destroy_virtual_display', {'display_id': first['display_id'], 'capture_generation': first['capture_generation']})
            assert stale.get('ok') is False, stale
            report['checks']['stale_generation_refused'] = True
            destroy(second)
            third = create()
            report['checks']['parent_eof_cleanup_requested'] = True
            terminate(daemon); daemon = None; active.clear()
    except Exception as error:
        report['error'] = str(error)
    finally:
        terminate(fixture)
        if daemon is not None and daemon.poll() is None and port and token:
            for monitor in list(active):
                try: destroy(monitor)
                except Exception: pass
        terminate(daemon)
        deadline = time.monotonic() + 10
        while True:
            report['after'] = inventory()
            if report['after'] == report['before'] or time.monotonic() >= deadline: break
            time.sleep(.1)
        report['inventory_restored'] = report['after'] == report['before']
        report['ok'] = 'error' not in report and report['inventory_restored']
        Path(args.report).write_text(json.dumps(report, indent=2))
        rig.cleanup()
        print(json.dumps(report, indent=2))
    return 0 if report['ok'] else 1

if __name__ == '__main__':
    raise SystemExit(main())
