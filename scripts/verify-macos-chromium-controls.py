#!/usr/bin/env python3
"""Opt-in native Chromium AX acceptance on an existing test-owned monitor.

Companion to verify-macos-monitor-http.py. The supplied native supervisor starts
ONLY Chrome for Testing with a fresh profile and owns cleanup. CDP sets up and
observes our synthetic fixture; it NEVER supplies text, clicks or canvas input.
All tested semantic mutations go through Intendant's real HTTP/facade interface.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import secrets
import selectors
import shutil
import socket
import struct
import subprocess
import tempfile
import time

from macos_input_evidence import assess_desktop


def require(value, detail):
    if not value:
        raise RuntimeError(str(detail))


def run_bounded(argv, deadline):
    """Drain both pipes under one byte/deadline cap; never retry an operation."""
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    output = bytearray()
    total = 0
    try:
        with selectors.DefaultSelector() as poll:
            poll.register(process.stdout, selectors.EVENT_READ, True)
            poll.register(process.stderr, selectors.EVENT_READ, False)
            while poll.get_map():
                require(time.monotonic() < deadline, 'ctl deadline; effects may be unconfirmed')
                for key, _ in poll.select(timeout=.05):
                    data = os.read(key.fileobj.fileno(), 4096)
                    if not data:
                        poll.unregister(key.fileobj)
                        continue
                    total += len(data)
                    require(total <= 65536, 'ctl output limit')
                    if key.data:
                        output.extend(data)
        require(process.wait(timeout=2) == 0, 'ctl failed; effects may be unconfirmed')
        return bytes(output)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=2)
        process.stdout.close()
        process.stderr.close()


class CDP:
    """Small bounded WebSocket client for our loopback-only test browser."""
    def __init__(self, port, path):
        require(1 <= port <= 65535 and re.fullmatch(r'/devtools/browser/[A-Za-z0-9-]+', path),
                'invalid private CDP address')
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=5)
        self.sock.settimeout(5)
        self.buffer = b''
        self.seq = 0
        self.read_deadline = time.monotonic() + 5
        try:
            self.handshake(port, path)
        except BaseException:
            self.sock.close()
            raise

    def handshake(self, port, path):
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        self.sock.sendall((f'GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n'
                           f'Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n'
                           'Sec-WebSocket-Version: 13\r\n\r\n').encode())
        while b'\r\n\r\n' not in self.buffer:
            require(len(self.buffer) < 8192, 'CDP handshake limit')
            remaining = self.read_deadline - time.monotonic()
            require(remaining > 0, 'CDP handshake deadline')
            self.sock.settimeout(min(5, remaining))
            data = self.sock.recv(4096)
            require(data, 'CDP handshake closed')
            self.buffer += data
        header, self.buffer = self.buffer.split(b'\r\n\r\n', 1)
        expected = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
        require(header.split(b'\r\n')[0].split()[1] == b'101', 'CDP handshake refused')
        headers = dict(line.decode().split(':', 1) for line in header.split(b'\r\n')[1:])
        require(any(k.lower() == 'sec-websocket-accept' and v.strip() == expected for k, v in headers.items()), 'CDP handshake key mismatch')

    def exact(self, count):
        while len(self.buffer) < count:
            remaining = self.read_deadline - time.monotonic()
            require(remaining > 0, 'CDP read deadline')
            self.sock.settimeout(min(5, remaining))
            data = self.sock.recv(min(65536, count - len(self.buffer)))
            require(data, 'CDP connection closed')
            self.buffer += data
        result, self.buffer = self.buffer[:count], self.buffer[count:]
        return result

    def send(self, payload, opcode=1):
        require(len(payload) <= 1024 * 1024, 'CDP send limit')
        mask = secrets.token_bytes(4)
        size = len(payload)
        length = bytes([0x80 | size]) if size < 126 else (b'\xfe' + struct.pack('!H', size) if size <= 65535 else b'\xff' + struct.pack('!Q', size))
        self.sock.sendall(bytes([0x80 | opcode]) + length + mask + bytes(v ^ mask[i % 4] for i, v in enumerate(payload)))

    def call(self, method, params=None, session=None):
        self.seq += 1
        request = {'id': self.seq, 'method': method, 'params': params or {}}
        if session:
            request['sessionId'] = session
        self.send(json.dumps(request).encode())
        deadline = time.monotonic() + 10
        self.read_deadline = deadline
        for _ in range(256):
            require(time.monotonic() < deadline, 'CDP response deadline')
            a, b = self.exact(2)
            require(a & 0x80 and not b & 0x80 and not a & 0x70, 'unsupported CDP frame')
            size = b & 127
            if size == 126:
                size = struct.unpack('!H', self.exact(2))[0]
            elif size == 127:
                size = struct.unpack('!Q', self.exact(8))[0]
            require(size <= 1024 * 1024, 'CDP receive limit')
            payload = self.exact(size)
            if a & 15 == 9:
                self.send(payload, 10)
                continue
            require(a & 15 == 1, 'CDP closed or non-text frame')
            reply = json.loads(payload)
            if reply.get('id') == self.seq:
                require('error' not in reply, reply)
                return reply['result']
        raise RuntimeError('CDP event limit')

    def close(self):
        self.sock.close()


def list_ready_fixture_window(call, status, initial, pause=time.sleep):
    """At most three read-only discovery calls; never change the selected window."""
    pid = initial['browser_pid']
    require(len(initial['windows']) == 1, 'browser window not unique')
    window_id = initial['windows'][0]['window_id']
    attempts = []
    candidates = []
    for attempt in range(3):
        listed = call('list_macos_windows', pid=pid)
        attempts.append(listed)
        candidates = [w for w in listed.get('candidates', []) if w['identity']['window_id'] == window_id]
        if len(candidates) == 1:
            break
        if listed.get('error') not in (None, 'application does not expose bounded AX windows'):
            break
        if attempt < 2:
            pause(.1)
            current = status()
            require(current['browser_pid'] == pid and len(current['windows']) == 1
                    and current['windows'][0]['window_id'] == window_id,
                    'browser window changed during readiness')
    return listed, candidates, attempts


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bin', required=True, type=Path)
    p.add_argument('--browser-app', required=True, type=Path)
    p.add_argument('--supervisor', required=True, type=Path)
    p.add_argument('--port', required=True, type=int)
    p.add_argument('--monitor', required=True)
    p.add_argument('--report', required=True, type=Path)
    p.add_argument('--allow-disposable-chromium', action='store_true')
    p.add_argument('--placement-only', action='store_true', help='Verify placement/no-op only; do not claim semantic actions')
    args = p.parse_args()
    if platform.system() != 'Darwin' or not args.allow_disposable_chromium or os.getenv('INTENDANT_MCP_URL'):
        p.error('requires macOS owner shell and explicit disposable Chromium opt-in')
    binary = args.bin.resolve(strict=True)
    bundle = args.browser_app.resolve(strict=True)
    supervisor = args.supervisor.resolve(strict=True)
    require(1 <= args.port <= 65535 and args.monitor.startswith('macos_virtual:'), 'exact monitor and port required')
    info = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
    require(info['CFBundleIdentifier'] == 'com.google.chrome.for.testing', 'only Chrome for Testing accepted')
    fixture_page = Path(__file__).resolve().parent.parent / 'tests/fixtures/macos-monitor/browser.html'
    report = {'passed': False, 'browser_version': info['CFBundleShortVersionString'],
              'browser_mode': 'background-window', 'profile': 'placement_only' if args.placement_only else 'semantic_controls',
              'checks': {}, 'cleanup': {}}
    root = Path(tempfile.mkdtemp(prefix='intendant-chromium-'))
    binding = None
    child = None
    cdp = None
    end = time.monotonic() + 150
    status_path = root / 'status.json'

    def call(tool, **arguments):
        require(time.monotonic() < end, 'acceptance deadline; do not retry mutations')
        return json.loads(run_bounded([str(binary), 'ctl', '--port', str(args.port), '--json',
                                      'tools', 'call', tool, '--args', json.dumps(arguments)],
                                     min(end, time.monotonic() + 25)))

    last_tick = -1

    def status():
        nonlocal last_tick
        until = time.monotonic() + 2
        while status_path.exists() and time.monotonic() < until:
            require(status_path.stat().st_size <= 16384, 'supervisor output limit')
            value = json.loads(status_path.read_text())
            if value.get('tick', -1) > last_tick:
                break
            time.sleep(.02)
        require(child and child.poll() is None and status_path.exists(), 'browser supervisor unavailable')
        require(status_path.stat().st_size <= 16384, 'supervisor output limit')
        value = json.loads(status_path.read_text())
        require(value['supervisor_pid'] == child.pid and not value.get('browser_terminated'), 'browser ownership/liveness changed')
        require(value.get('tick', -1) > last_tick, 'stale supervisor observation')
        last_tick = value['tick']
        require(not value.get('browser_ever_front'), 'browser was observed foreground')
        return value

    try:
        owned = call('list_macos_monitors').get('monitors', [])
        require(sum(m['display_target'] == args.monitor for m in owned) == 1, 'monitor not owned by isolated daemon')
        profile = root / 'profile'
        profile.mkdir(mode=0o700)
        child = subprocess.Popen([str(supervisor), '--disposable-chromium', str(bundle), str(profile), str(status_path), fixture_page.as_uri()],
                                 stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        active = profile / 'DevToolsActivePort'
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline and child.poll() is None:
            if active.exists() and status_path.exists():
                break
            time.sleep(.05)
        require(active.exists(), 'private browser did not expose its own CDP endpoint')
        require(active.stat().st_size <= 8192, 'CDP address file limit')
        port, path = active.read_text().splitlines()[:2]
        cdp = CDP(int(port), path)
        current = status()
        report['observations_before_launch'] = current['before']
        processes = cdp.call('SystemInfo.getProcessInfo')['processInfo']
        require(any(x['type'] == 'browser' and x['id'] == current['browser_pid'] for x in processes), 'CDP browser differs from retained supervisor child')
        cdp.call('Target.createTarget', {'url': fixture_page.as_uri(), 'newWindow': True,
                                       'background': True, 'width': 720, 'height': 530})
        targets = []
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            targets = [t for t in cdp.call('Target.getTargets')['targetInfos'] if t['type'] == 'page' and t['url'] == fixture_page.as_uri()]
            if len(targets) == 1:
                break
            time.sleep(.05)
        require(len(targets) == 1, 'synthetic fixture page not unique')
        session = cdp.call('Target.attachToTarget', {'targetId': targets[0]['targetId'], 'flatten': True})['sessionId']

        def evaluate(expression):
            result = cdp.call('Runtime.evaluate', {'expression': expression, 'returnByValue': True}, session)
            require('exceptionDetails' not in result, 'fixture evaluation failed')
            return result['result'].get('value')

        ready_deadline = time.monotonic() + 10
        while time.monotonic() < ready_deadline:
            if evaluate("document.readyState === 'complete' && typeof fixtureStatus === 'function'"):
                break
            time.sleep(.05)
        require(evaluate("typeof fixtureStatus === 'function'"), 'synthetic fixture not ready')
        initial = evaluate('fixtureStatus()')
        require(initial['button_count'] == initial['canvas_count'] == 0, 'fixture is not pristine')
        current = status()
        report['observations_after_launch'] = current['observation']
        report['browser_pid'] = current['browser_pid']
        require(current['observation']['front_pid'] != current['browser_pid'], 'browser became foreground during launch')
        window_deadline = time.monotonic() + 5
        while len(current['windows']) == 0 and time.monotonic() < window_deadline:
            time.sleep(.05)
            current = status()
        require(len(current['windows']) == 1, 'browser window not unique')
        listed, candidates, attempts = list_ready_fixture_window(call, status, current)
        report['checks']['window_listing'] = listed
        report['checks']['window_listing_attempts'] = attempts
        require(len(candidates) == 1, listed)
        bound = call('bind_macos_window', display_target=args.monitor, **{k: candidates[0][k] for k in ('candidate', 'identity')})
        require(bound.get('ok') is True, bound)
        binding = bound['bound_window']['binding']
        placed = call('place_macos_window', binding=binding, bounds={'x': 30, 'y': 30, 'width': 720, 'height': 530})
        report['checks']['placement'] = placed
        require(placed.get('ok') is True, placed)
        native = placed['placement']
        require(native['writes_attempted'] == 1, 'move-only fixture must not call the size setter')
        browser_bounds = cdp.call('Browser.getWindowForTarget', {'targetId': targets[0]['targetId']})['bounds']
        expected = native['requested_global']
        require(all(abs(browser_bounds[a] - expected[b]) <= 1 for a, b in
                    [('left', 'x'), ('top', 'y'), ('width', 'width'), ('height', 'height')]),
                'independent browser window geometry disagrees')
        report['checks']['independent_browser_geometry'] = browser_bounds
        noop = call('place_macos_window', binding=binding, bounds={'x': 30, 'y': 30, 'width': 720, 'height': 530})
        report['checks']['no_op_placement'] = noop
        require(noop.get('ok') is True and noop['placement']['writes_attempted'] == 0, noop)


        if not args.placement_only:
            def read():
                result = call('read_macos_window_elements', binding=binding)
                report['last_inventory'] = result
                if result.get('ok') is not True:
                    child.stdin.write(b'd'); child.stdin.flush()
                    until = time.monotonic() + 8
                    while time.monotonic() < until:
                        observed = status()
                        if 'tree_diagnostic' in observed:
                            report['tree_diagnostic'] = observed['tree_diagnostic']
                            break
                        time.sleep(.05)
                require(result.get('ok') is True, result)
                controls = result['controls']
                require(all(set(c) == {'element', 'role', 'label', 'bounds', 'operations'} for c in controls), 'unexpected public fields')
                wire = json.dumps(controls)
                require('synthetic-secret' not in wire and 'Omit secure' not in wire and 'Omit disabled' not in wire, 'excluded control leaked')
                fields = [c for c in controls if c['label'] == 'Normal browser field' and c['role'] == 'AXTextField']
                buttons = [c for c in controls if c['label'] == 'Increment browser counter' and c['role'] == 'AXButton']
                require(len(fields) == len(buttons) == 1, 'normal browser controls not uniquely described')
                return fields[0]['element'], buttons[0]['element']

            def action(element, value):
                return call('act', argv=['display', 'window-element', binding, element, json.dumps(value)])

            old, _ = read()
            read()
            rejected = action(old, {'type': 'set_value', 'text': 'must not apply'})
            require(rejected.get('ok') is False and rejected.get('action_attempted') is False, rejected)
            require(evaluate('fixtureStatus()') == initial, 'stale token changed fixture')
            report['checks']['refresh_refused'] = True
            field, _ = read()
            text = 'Chromium exact text π 😀'
            result = action(field, {'type': 'set_value', 'text': text})
            report['checks']['text_action'] = result
            report['checks']['text_independent_readback'] = evaluate('fixtureStatus().text') == text
            require(result.get('ok') is True and result['action']['status'] == 'verified', result)
            require(evaluate('fixtureStatus().text') == text, 'independent DOM text disagrees')
            replay = action(field, {'type': 'set_value', 'text': 'must not replay'})
            require(replay.get('ok') is False and replay.get('action_attempted') is False and evaluate('fixtureStatus().text') == text, replay)
            report['checks']['text_independent_and_replay'] = True
            _, button = read()
            result = action(button, {'type': 'press'})
            report['checks']['press_action'] = result
            report['checks']['button_independent_count'] = evaluate('fixtureStatus().button_count')
            require(result.get('ok') is True and result['action']['status'] == 'dispatched' and result['effects_unconfirmed'], result)
            require(evaluate('fixtureStatus().button_count') == 1, 'button effect not observed')
            replay = action(button, {'type': 'press'})
            require(replay.get('ok') is False and replay.get('action_attempted') is False and evaluate('fixtureStatus().button_count') == 1, replay)
            report['checks']['button_exactly_once'] = True
            field, _ = read()
            evaluate('replaceFixtureField(); true')
            stale = action(field, {'type': 'set_value', 'text': 'must not target replacement'})
            require(stale.get('ok') is False and stale.get('action_attempted') is False, stale)
            require(evaluate('fixtureStatus().text') == 'replacement disposable text', 'replacement acted on')
            report['checks']['replacement_refused'] = True
            # Negative action test: replace the document in OUR disposable page.
            # CDP does not provide the text/button actions being tested.
            field, _ = read()
            cdp.call('Page.reload', {}, session)
            until = time.monotonic() + 5
            ready = False
            while time.monotonic() < until:
                ready = evaluate("document.readyState === 'complete' && typeof fixtureStatus === 'function' && fixtureStatus().text === 'initial disposable text' && fixtureStatus().button_count === 0")
                if ready:
                    break
                time.sleep(.05)
            require(ready, 'replacement document not ready')
            stale = action(field, {'type': 'set_value', 'text': 'must not cross navigation'})
            require(stale.get('ok') is False and stale.get('action_attempted') is False, stale)
            require(evaluate('fixtureStatus().text') == 'initial disposable text', 'stale control crossed navigation')
            report['checks']['navigation_refused'] = True
            require(evaluate('fixtureStatus().canvas_count') == 0, 'unexpected canvas input')
            report['checks']['canvas_input'] = 'not implemented or exercised; semantic controls are not raw canvas input'
        else:
            report['checks']['semantic_controls'] = 'not exercised by placement-only profile'
        current = status()
        report['observations_after'] = current['observation']
        report['human_observations_unchanged'] = current['before'] == current['observation']
        require(current['observation']['front_pid'] != current['browser_pid'], 'browser became foreground during actions')
        report['passed'] = True
    except Exception as error:
        report['error'] = str(error)
    finally:
        end = time.monotonic() + 30
        if binding:
            try:
                unbound = call('unbind_macos_window', binding=binding)
                report['cleanup']['unbind'] = unbound
                require(unbound.get('ok') is True, unbound)
            except Exception as error:
                report['cleanup']['unbind_error'] = str(error)
                report['passed'] = False
        if cdp:
            cdp.close()
        if child:
            try:
                if child.poll() is None:
                    child.stdin.write(b'q'); child.stdin.flush(); child.stdin.close()
                child.wait(timeout=12)
                require(not status_path.exists() or status_path.stat().st_size <= 16384,
                        'final supervisor output limit')
                final = json.loads(status_path.read_text()) if status_path.exists() else {}
                report['cleanup']['browser_terminated'] = (final.get('supervisor_pid') == child.pid
                    and final.get('launch_finished') is True and final.get('browser_pid', 0) > 0
                    and final.get('browser_terminated') is True)
                report['cleanup']['supervisor_reaped'] = child.returncode is not None
                report['cleanup']['browser_ever_front'] = final.get('browser_ever_front')
                require(final.get('supervisor_pid') == child.pid and final.get('launch_finished') is True
                        and final.get('browser_pid', 0) > 0 and final.get('browser_terminated') is True,
                        'browser cleanup unconfirmed')
            except Exception as error:
                report['cleanup']['error'] = str(error)
                report['passed'] = False
                # The native supervisor owns exact-browser termination and has
                # its own bounded lifetime. Do not kill an unverified PID.
        if report['cleanup'].get('browser_terminated') or not child:
            try:
                shutil.rmtree(root)
                report['cleanup']['profile_removed'] = True
            except OSError as error:
                report['cleanup']['profile_error'] = str(error)
                report['passed'] = False
        else:
            report['cleanup']['profile_retained'] = str(root)
        report['desktop_observation_assessment'] = assess_desktop(
            report.get('observations_before_launch'), report.get('observations_after'),
            report.get('browser_pid'), report['cleanup'].get('browser_ever_front'))
        args.report.write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
