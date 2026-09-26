#!/usr/bin/env python3
"""Opt-in native Chromium AX acceptance on an existing test-owned monitor.

Companion to verify-macos-monitor-http.py. The supplied native supervisor starts
ONLY Chrome for Testing with a fresh profile and owns cleanup. CDP sets up and
observes our synthetic fixture; it NEVER supplies text, clicks or canvas input.
All tested semantic mutations go through Intendant's real HTTP/facade interface.

The --keyboard-target profile uses disposable monitor/window placement as setup,
then tests the read-only receiver API. CDP selects only a field in its own local
page; the profile sends no native keys or system-wide focus operations. Its report is not a keyboard-delivery claim.
"""
import math
import macos_arrow_acceptance as arrow_acceptance
import macos_concurrent_keys as concurrent_keys
import macos_receiver_study as receiver_study
import macos_focus_witness as focus_study
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


def run_bounded_observed(argv, deadline, observe):
    # One ctl process; collect read-only samples only while it remains alive.
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    output = bytearray()
    total = 0
    samples = []
    try:
        with selectors.DefaultSelector() as poll:
            poll.register(process.stdout, selectors.EVENT_READ, True)
            poll.register(process.stderr, selectors.EVENT_READ, False)
            while poll.get_map():
                require(time.monotonic() < deadline, 'ctl deadline; effects may be unconfirmed')
                if process.poll() is None:
                    value = observe()
                    if process.poll() is None and value is not None:
                        samples.append(value)
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
        return bytes(output), samples
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



def validate_keyboard_receiver(target, state, window):
    require(isinstance(target, dict) and set(target) == {'role', 'bounds', 'enabled', 'keyboard_dispatch_supported'}, 'unexpected keyboard receiver fields')
    require(target['role'] == 'AXTextField' and target['enabled'] is True and target['keyboard_dispatch_supported'] is False, 'receiver role or capability mismatch')
    return validate_keyboard_receiver_bounds(target['bounds'], state, window)

def validate_prepared_keyboard_receiver(receiver, state, window):
    # A frozen preparation has three fields, unlike the read-only observation.
    require(isinstance(receiver, dict) and set(receiver) == {'role', 'bounds', 'enabled'}, 'unexpected prepared receiver fields')
    require(receiver['role'] == 'AXTextField' and receiver['enabled'] is True, 'prepared receiver role or enabled mismatch')
    return validate_keyboard_receiver_bounds(receiver['bounds'], state, window)

def validate_keyboard_receiver_bounds(bounds, state, window):
    def number(value):
        return type(value) in (int, float) and math.isfinite(value) and abs(value) <= 1000000
    require(isinstance(state, dict) and state.get("active") in ("first", "second"), "ordinary synthetic receiver required")
    rect, metrics = state.get("rect"), state.get("metrics")
    for geometry in (rect, bounds, window):
        require(isinstance(geometry, dict) and set(geometry) == {"x", "y", "width", "height"} and all(number(v) for v in geometry.values()) and geometry["width"] > 0 and geometry["height"] > 0, "invalid receiver geometry")
    keys = ("screen_x", "screen_y", "outer_width", "outer_height", "inner_width", "inner_height", "scale", "zoom", "scroll_x", "scroll_y")
    require(isinstance(metrics, dict) and all(number(metrics.get(k)) for k in keys), "invalid fixture metrics")
    require(metrics["scale"] == metrics["zoom"] == 1 and metrics["scroll_x"] == metrics["scroll_y"] == 0, "unsupported fixture zoom or scroll")
    require(metrics["inner_width"] == metrics["outer_width"], "unsupported horizontal browser border")
    require(all(abs(metrics[a] - window[b]) <= 1 for a,b in (("screen_x","x"),("screen_y","y"),("outer_width","width"),("outer_height","height"))), "native and DOM window geometry disagree")
    top = metrics["outer_height"] - metrics["inner_height"]
    require(0 <= top <= 200, "invalid browser content origin")
    expected = dict(x=window["x"]+rect["x"], y=window["y"]+top+rect["y"], width=rect["width"], height=rect["height"])
    require(all(abs(bounds[k]-expected[k]) <= 1 for k in expected), "AX receiver differs from independently focused DOM field")
    return expected


def keyboard_setup_plan(state, window):
    """Plan only the first nonsecret field in the disposable receiver fixture."""
    require(isinstance(state, dict) and state.get('active') == 'first', 'first fixture field required')
    rect, metrics = state.get('rect', {}), state.get('metrics', {})
    keys = ('screen_x', 'screen_y', 'outer_width', 'outer_height', 'inner_width', 'inner_height', 'scale', 'zoom', 'scroll_x', 'scroll_y')
    def finite(v):
        try:
            return type(v) in (int, float) and abs(v) <= 1000000 and math.isfinite(v)
        except OverflowError:
            return False
    require(all(finite(rect.get(k)) and finite(window.get(k)) for k in ('x', 'y', 'width', 'height'))
            and all(finite(metrics.get(k)) for k in keys), 'invalid fixture geometry')
    require(metrics['scale'] == metrics['zoom'] == 1 and metrics['scroll_x'] == metrics['scroll_y'] == 0, 'fixture zoom or scroll changed')
    require(all(abs(metrics[a] - window[b]) <= 1 for a,b in (('screen_x','x'), ('screen_y','y'), ('outer_width','width'), ('outer_height','height'))), 'fixture window changed')
    require(metrics['inner_width'] == metrics['outer_width'], 'unsupported browser border')
    top = metrics['outer_height'] - metrics['inner_height']
    require(0 <= top <= 200 and rect['width'] >= 20 and rect['height'] >= 20
            and 0 <= rect['x'] and 0 <= rect['y']
            and rect['x'] + rect['width'] <= metrics['inner_width']
            and rect['y'] + rect['height'] <= metrics['inner_height'], 'fixture field outside viewport')
    client = {'x': rect['x'] + rect['width'] * .37, 'y': rect['y'] + rect['height'] * .61}
    point = {'x': client['x'], 'y': top + client['y']}
    return {'client': client, 'point': point,
            'global': {k: point[k] + window[k] for k in ('x', 'y')}}


def validate_keyboard_setup_click(plan, native, before, after):
    require(isinstance(native, dict) and native.get('ok') is True, 'setup click refused or uncertain')
    action = native.get('action', {})
    require(action.get('status') == 'dispatched' and type(action.get('posting_calls')) is int and action['posting_calls'] == 2
            and action.get('action_attempted') is True and action.get('effect_verified') is False
            and action.get('effects_unconfirmed') is True and action.get('focus_interference') is False
            and action.get('detail') is None, 'setup dispatch not established')
    require(action.get('point') == plan['point'] and action.get('global') == plan['global'], 'setup point mismatch')
    require(after.get('active') == 'first' and after.get('metrics') == before['metrics']
            and after.get('rect') == before['rect'], 'setup receiver changed')
    events = after.get('click_events')
    require(after.get('click_overflow') is False and isinstance(events, list) and len(events) == 3, 'setup click not observed exactly once')
    for event, kind, buttons in zip(events, ('mousedown','mouseup','click'), (1,0,0)):
        require(isinstance(event, dict) and event.get('kind') == kind and event.get('target') == 'first'
                and event.get('trusted') is True and type(event.get('button')) is int and event['button'] == 0
                and type(event.get('buttons')) is int and event['buttons'] == buttons
                and all(event.get(k) is False for k in ('alt','control','meta','shift')), 'invalid setup click evidence')
        for key, value in (('client_x',plan['client']['x']), ('client_y',plan['client']['y']),
                           ('screen_x',plan['global']['x']), ('screen_y',plan['global']['y'])):
            observed = event.get(key)
            require(type(observed) in (int,float) and abs(observed) <= 1000000 and math.isfinite(observed)
                    and abs(observed-value) <= 1, 'setup click at wrong point')


def select_keyboard_fixture_with_click(call, evaluate, binding, state, window, evidence):
    """Explicit test setup only. Inspection itself never clicks or retries."""
    require(state.get('click_events') == [] and state.get('click_overflow') is False, 'setup mouse evidence not pristine')
    plan = keyboard_setup_plan(state, window)
    evidence['plan'] = plan
    prepared = call('prepare_macos_window_click', binding=binding, point=plan['point'])
    evidence['preparation'] = prepared
    require(prepared.get('ok') is True, prepared)
    frozen = prepared.get('prepared', {})
    require(frozen.get('point') == plan['point'] and frozen.get('global') == plan['global']
            and frozen.get('window') == {'ax':window, 'cg':window}, 'setup preparation mismatch')
    token = frozen.get('token')
    require(isinstance(token,str) and len(token) == 46 and token.startswith('macos_pointer:')
            and all(c in '0123456789abcdef' for c in token[14:]), 'invalid setup token')
    native = call('click_macos_window', binding=binding, token=token)
    evidence['dispatch'] = native
    # Preserve available effects even after possible partial dispatch. No replay.
    deadline = time.monotonic() + 2
    observed = evaluate('keyboardTargetFixtureState()')
    while len(observed.get('click_events', [])) < 3 and time.monotonic() < deadline:
        time.sleep(.05)
        observed = evaluate('keyboardTargetFixtureState()')
    evidence['observed'] = observed
    validate_keyboard_setup_click(plan, native, state, observed)
    evidence.update(effect_verified=True, dom_tag_correlation=False)
    return observed


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
    p.add_argument('--keyboard-target', action='store_true', help='Run only the read-only focused-receiver inspection profile')
    p.add_argument('--keyboard-target-click-first', action='store_true', help='Explicit single bound-window setup click on the first disposable nonsecret field')
    p.add_argument('--arrowright',action='store_true',help='Explicit one-pair production ArrowRight acceptance after verified setup selection')
    p.add_argument('--arrowleft',action='store_true',help='Explicit fixed ArrowLeft acceptance; requires separate setup click')
    p.add_argument("--receiver-study", action="store_true", help="Fixed twelve-read receiver study; no keys; requires explicit setup click")
    p.add_argument('--native-focus', action='store_true', help='Native focus bracketing; requires receiver-study')
    p.add_argument("--concurrent-key-study", action="store_true", help="One arrow attempt with passive native focus/HID evidence")
    p.add_argument("--require-mouse-activity", action="store_true", help="Require mouse-motion counter progress before and while the one ctl dispatch process is alive")
    p.add_argument("--require-keyboard-activity", action="store_true", help="Require aggregate HID key-down/key-up activity around the one ctl dispatch; records no key identity or text")
    args = p.parse_args()
    require(not args.require_mouse_activity or args.concurrent_key_study,
            'mouse activity requires concurrent-key study')
    require(not args.require_keyboard_activity or args.concurrent_key_study,
            'keyboard activity requires concurrent-key study')
    require(not (args.require_mouse_activity and args.require_keyboard_activity),
            'choose at most one required activity profile')
    try:
        concurrent_keys.validate_options(args.concurrent_key_study, args.keyboard_target,
            args.keyboard_target_click_first, args.arrowleft, args.arrowright,
            args.receiver_study or args.native_focus or args.placement_only)
    except ValueError as error:
        p.error(str(error))
    require(not args.native_focus or args.receiver_study, 'native focus requires receiver-study')
    try:
        receiver_study.validate_options(args.receiver_study, args.keyboard_target, args.keyboard_target_click_first, args.arrowleft or args.arrowright, args.placement_only)
    except ValueError as error:
        p.error(str(error))
    require(not (args.arrowleft and args.arrowright), 'choose exactly one arrow profile')
    arrow_key = 'ArrowLeft' if args.arrowleft else 'ArrowRight'
    arrow_profile = args.arrowleft or args.arrowright
    require(not arrow_profile or (args.keyboard_target and args.keyboard_target_click_first), 'ArrowRight test requires explicit keyboard-target and click-first setup')
    require(not args.keyboard_target_click_first or args.keyboard_target, 'click-first requires keyboard-target')
    if platform.system() != 'Darwin' or not args.allow_disposable_chromium or os.getenv('INTENDANT_MCP_URL'):
        p.error('requires macOS owner shell and explicit disposable Chromium opt-in')
    binary = args.bin.resolve(strict=True)
    bundle = args.browser_app.resolve(strict=True)
    supervisor = args.supervisor.resolve(strict=True)
    require(1 <= args.port <= 65535 and args.monitor.startswith('macos_virtual:'), 'exact monitor and port required')
    require(not (args.placement_only and args.keyboard_target), 'keyboard target excludes placement-only profile')
    info = plistlib.loads((bundle / 'Contents/Info.plist').read_bytes())
    require(info['CFBundleIdentifier'] == 'com.google.chrome.for.testing', 'only Chrome for Testing accepted')
    fixture_page = Path(__file__).resolve().parent.parent / 'tests/fixtures/macos-monitor/' / (
        'browser-keyboard-target.html' if args.keyboard_target else 'browser.html')
    report = {'passed': False, 'browser_version': info['CFBundleShortVersionString'],
              'browser_mode': 'background-window',
              'profile': 'mouse_overlap' if args.require_mouse_activity else 'keyboard_overlap' if args.require_keyboard_activity else 'concurrent_key' if args.concurrent_key_study else 'native_focus' if args.native_focus else 'receiver_study' if args.receiver_study else 'keyboard_target' if args.keyboard_target else ('placement_only' if args.placement_only else 'semantic_controls'),
              'checks': {}, 'cleanup': {}}
    root = Path(tempfile.mkdtemp(prefix='intendant-chromium-'))
    binding = None
    child = None
    cdp = None
    ownership_verified = False
    end = time.monotonic() + 150
    status_path = root / 'status.json'

    def ctl_argv(tool, arguments):
        return [str(binary), 'ctl', '--port', str(args.port), '--json',
                'tools', 'call', tool, '--args', json.dumps(arguments)]

    def call(tool, **arguments):
        require(time.monotonic() < end, 'acceptance deadline; do not retry mutations')
        return json.loads(run_bounded(ctl_argv(tool, arguments),
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
        child = subprocess.Popen([str(supervisor), '--disposable-chromium-concurrent-key' if args.concurrent_key_study else '--disposable-chromium-focus' if args.native_focus else '--disposable-chromium', str(bundle), str(profile), str(status_path), fixture_page.as_uri()],
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
        ownership_verified = True
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
            fixture_ready = ('typeof keyboardTargetFixtureState === \'function\'' if args.keyboard_target
                             else 'typeof fixtureStatus === \'function\'')
            if evaluate(f"document.readyState === 'complete' && {fixture_ready}"):
                break
            time.sleep(.05)
        require(evaluate(fixture_ready), 'synthetic fixture not ready')
        initial = evaluate('keyboardTargetFixtureState()' if args.keyboard_target else 'fixtureStatus()')
        if args.keyboard_target:
            require(initial['active'] is None and initial['ready'] == 'complete', 'keyboard fixture is not pristine')
        else:
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

        if args.keyboard_target:
            def read_receiver(state):
                result = call('read_macos_window_keyboard_target', binding=binding)
                require(result.get('ok') is True, result)
                target = result.get('keyboard_target')
                require(isinstance(target, dict) and set(target) == {
                    'role', 'bounds', 'enabled', 'keyboard_dispatch_supported'
                }, 'unexpected keyboard-target report fields')
                require(target['role'] == 'AXTextField' and target['enabled'] is True,
                        'fixture receiver role or enabled state mismatched')
                require(target['keyboard_dispatch_supported'] is False, target)
                bounds = target['bounds']
                require(set(bounds) == {'x', 'y', 'width', 'height'} and bounds['width'] > 0 and bounds['height'] > 0,
                        'receiver geometry invalid')
                wire = json.dumps(result)
                require(all(forbidden not in wire for forbidden in (
                    'synthetic-secret', 'fixture-a', 'fixture-b', 'label', 'value', 'token', 'pointer')),
                    'receiver report leaked content or capability')
                validate_keyboard_receiver(target, state, expected)
                return target

            first_state = evaluate("selectKeyboardTarget('first')")
            require(first_state['active'] == 'first', 'fixture-only DOM focus setup failed for first field')
            if args.keyboard_target_click_first:
                report['checks']['explicit_setup_click'] = {}
                first_state = select_keyboard_fixture_with_click(call, evaluate, binding, first_state, expected, report['checks']['explicit_setup_click'])
            if args.concurrent_key_study:
                series = report['concurrent_key_study'] = {}
                series['mouse_activity_required'] = args.require_mouse_activity
                series['keyboard_activity_required'] = args.require_keyboard_activity
                report['passed_semantics'] = 'collection/evidence validation; effect and activity are reported separately'
                report['checks']['keyboard_input_requested'] = True
                def checkpoint():
                    pending = args.report.with_name(args.report.name + '.partial')
                    pending.write_text(json.dumps(report, indent=2) + chr(10))
                    pending.replace(args.report)
                def witness(phase, sequence):
                    child.stdin.write(b'f' if phase == 'before' else b'g'); child.stdin.flush()
                    until = min(end, time.monotonic() + 3)
                    while time.monotonic() < until:
                        value = status().get('focus_witness', {})
                        if value.get('sequence') == sequence and value.get('phase') == phase:
                            return value
                        require(value.get('phase') != 'refused', 'native witness refused')
                        time.sleep(.01)
                    raise RuntimeError('native witness acknowledgement deadline')
                def current_activity(sequence):
                    value = status().get('activity_current')
                    return concurrent_keys.validate_current_activity(value, sequence)
                def wait_for_mouse(sequence):
                    until = min(end, time.monotonic() + 8)
                    while time.monotonic() < until:
                        value = current_activity(sequence)
                        activity = value['hid_activity']
                        if not activity['counter_regression'] and activity['deltas']['mouse_move'] > 0:
                            return value
                    raise RuntimeError('required mouse activity not observed before dispatch')
                def wait_for_keyboard_pair(sequence):
                    until = min(end, time.monotonic() + 8)
                    while time.monotonic() < until:
                        value = current_activity(sequence)
                        activity = value['hid_activity']
                        if (not activity['counter_regression']
                                and activity['deltas']['key_down'] > 0
                                and activity['deltas']['key_up'] > 0):
                            return value
                    raise RuntimeError('required completed keyboard activity not observed before dispatch')
                before_activity = None
                def before_dispatch():
                    nonlocal before_activity
                    if args.require_mouse_activity:
                        before_activity = wait_for_mouse(2)
                    elif args.require_keyboard_activity:
                        before_activity = wait_for_keyboard_pair(2)
                def study_call(tool, **arguments):
                    required_activity = args.require_mouse_activity or args.require_keyboard_activity
                    if not required_activity or not tool.startswith('press_macos_window_'):
                        return call(tool, **arguments)
                    require(before_activity is not None, 'missing pre-dispatch activity gate')
                    payload, samples = run_bounded_observed(
                        ctl_argv(tool, arguments), min(end, time.monotonic() + 25),
                        lambda: current_activity(2))
                    reply = json.loads(payload)
                    series['dispatch_client_activity'] = concurrent_keys.summarize_client_activity(
                        before_activity, samples)
                    checkpoint()
                    if args.require_mouse_activity:
                        series['mouse_overlap'] = concurrent_keys.validate_mouse_overlap(
                            before_activity, samples)
                    else:
                        series['keyboard_overlap'] = concurrent_keys.summarize_keyboard_overlap(
                            before_activity, samples)
                    checkpoint()
                    return reply
                concurrent_keys.collect(study_call, evaluate, binding, witness,
                    validate_prepared_keyboard_receiver, expected, series, checkpoint,
                    min(end, time.monotonic() + 40), arrow_key,
                    before_dispatch=before_dispatch)
                require(series['completed'] and series['measurement_valid'], 'concurrent key study incomplete')
                if args.require_mouse_activity:
                    failure = (series.get('preparation', {}).get('reply', {}).get('error')
                               or series.get('stop_reason') or series.get('outcome'))
                    require(series.get('outcome') == 'effect_verified',
                            'mouse overlap study did not verify key effect: ' + str(failure))
                    overlap = series.get('mouse_overlap', {})
                    require(overlap.get('before_dispatch', 0) > 0
                            and overlap.get('while_client_alive', 0) > 0
                            and overlap.get('client_process_overlap_verified') is True
                            and overlap.get('internal_posting_overlap_verified') is False,
                            'required mouse overlap evidence missing')
                elif args.require_keyboard_activity:
                    failure = (series.get('preparation', {}).get('reply', {}).get('error')
                               or series.get('stop_reason') or series.get('outcome'))
                    require(series.get('outcome') in ('dispatch_refused', 'effect_verified'),
                            'keyboard overlap study reached unsupported outcome: ' + str(failure))
                    series['keyboard_overlap'] = concurrent_keys.finalize_keyboard_overlap(
                        series['outcome'], series.get('dispatch', {}).get('reply'),
                        series.get('keyboard_overlap', {}),
                        series.get('dispatch', {}).get('native_after'))
                    series['summary'] = concurrent_keys.summarize(series)
                    checkpoint()
            elif args.receiver_study:
                series = report['receiver_study'] = {}
                report['passed_semantics'] = 'measurement collection and validation only; not input availability'
                report['checks']['keyboard_input_requested'] = False
                def study_state():
                    return evaluate('receiverStudyState()')
                def study_select(name):
                    require(name in ('first', 'second', 'protected'), 'unknown study field')
                    return evaluate("selectKeyboardTarget(" + json.dumps(name) + "); receiverStudyState()")
                def checkpoint():
                    pending = args.report.with_name(args.report.name + '.partial')
                    pending.write_text(json.dumps(report, indent=2) + chr(10))
                    pending.replace(args.report)
                if args.native_focus:
                    def witness(phase, sequence):
                        child.stdin.write(b'f' if phase == 'before' else b'g'); child.stdin.flush()
                        until = min(end, time.monotonic() + 3)
                        while time.monotonic() < until:
                            value = status().get('focus_witness', {})
                            if value.get('sequence') == sequence and value.get('phase') == phase:
                                return value
                            require(value.get('phase') != 'refused', 'native witness refused')
                            time.sleep(.01)
                        raise RuntimeError('native witness acknowledgement deadline')
                    focus_study.collect(
                        lambda: call('read_macos_window_keyboard_target', binding=binding),
                        study_select, study_state, witness,
                        lambda: evaluate('startFocusWitnessChurn()'),
                        lambda: evaluate('stopFocusWitnessChurn()'),
                        validate_keyboard_receiver, expected, series, checkpoint,
                        min(end, time.monotonic() + 75))
                else:
                    receiver_study.collect(
                        lambda: call('read_macos_window_keyboard_target', binding=binding),
                        study_select, study_state, lambda: status()['observation'],
                        validate_keyboard_receiver, expected, series, checkpoint,
                        min(end, time.monotonic() + 75))
                require(series['completed'] and series['measurement_valid'], 'receiver study incomplete')
            else:
                first_receiver = read_receiver(first_state)
                report['checks']['first_receiver'] = first_receiver
                pending_arrow = None
                if arrow_profile:
                    report[arrow_key.lower()] = {}
                    pending_arrow = arrow_acceptance.exercise(call,evaluate,binding,report[arrow_key.lower()],arrow_key)
                second_state = evaluate("selectKeyboardTarget('second')")
                require(second_state['active'] == 'second', 'fixture-only DOM focus setup failed for second field')
                second = call('inspect', argv=['display', 'keyboard-target', binding])
                require(second.get('ok') is True, second)
                second_receiver = second.get('keyboard_target')
                require(isinstance(second_receiver, dict) and set(second_receiver) == set(first_receiver), second)
                require(second_receiver['role'] == 'AXTextField' and second_receiver['enabled'] is True
                        and second_receiver['keyboard_dispatch_supported'] is False, second)
                require(second_receiver['bounds'] != first_receiver['bounds'],
                        'changed fixture receiver did not change reported geometry')
                report['checks']['changed_receiver'] = second_receiver
                validate_keyboard_receiver(second_receiver, second_state, expected)
                protected_state = evaluate("selectKeyboardTarget('protected')")
                require(protected_state['active'] == 'protected', 'fixture-only DOM focus setup failed for password field')
                protected = call('read_macos_window_keyboard_target', binding=binding)
                protected_wire = json.dumps(protected)
                require(protected.get('ok') is False and 'synthetic-secret' not in protected_wire, protected)
                report['checks']['protected_receiver_refused'] = True
                require(any(word in str(protected.get("error", "")).lower() for word in ("protected", "absent")), "protected refusal reason was not established")
                report["checks"]["protected_receiver_result"] = protected
                stale_binding = binding
                unbound = call('unbind_macos_window', binding=stale_binding)
                require(unbound.get('ok') is True, unbound)
                binding = None
                stale = call('read_macos_window_keyboard_target', binding=stale_binding)
                require(stale.get('ok') is False and 'stale' in json.dumps(stale).lower(), stale)
                report['checks']['stale_binding_refused'] = True
                if pending_arrow is not None:
                    refused=call('press_macos_window_'+arrow_key.lower(),binding=stale_binding,token=pending_arrow)
                    require(refused.get('ok') is False and refused.get('action_attempted') is False and refused.get('effects_unconfirmed') is False,refused)
                    report[arrow_key.lower()]['checks']['stale_binding_refused']=True
                report['checks']['fixture_setup'] = ('one explicit verified native click, then fixture-only DOM receiver changes' if args.keyboard_target_click_first else 'DOM focus only; no native click')
                report['checks']['keyboard_input_requested'] = arrow_profile
        elif not args.placement_only:
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
            if args.keyboard_target and ownership_verified:
                try:
                    cdp.call("Browser.close")
                except (OSError, RuntimeError):
                    pass  # Native termination, not a CDP acknowledgement, proves exit.
            try:
                cdp.close()
            except OSError as error:
                report["cleanup"]["cdp_error"] = str(error)
                report["passed"] = False
        if child:
            try:
                if child.poll() is None:
                    try:
                        child.stdin.write(b"q"); child.stdin.flush()
                    except (BrokenPipeError, OSError):
                        pass
                    finally:
                        try: child.stdin.close()
                        except (BrokenPipeError, OSError): pass
                child.wait(timeout=12)
                require(not status_path.exists() or status_path.stat().st_size <= 16384,
                        'final supervisor output limit')
                final = json.loads(status_path.read_text()) if status_path.exists() else {}
                if final.get('supervisor_pid') == child.pid:
                    report['observations_after'] = final.get('observation')
                report['cleanup']['browser_terminated'] = (final.get('supervisor_pid') == child.pid
                    and final.get('launch_finished') is True and final.get('browser_pid', 0) > 0
                    and final.get('browser_terminated') is True)
                report['cleanup']['supervisor_reaped'] = child.returncode is not None
                report['cleanup']['browser_ever_front'] = final.get('browser_ever_front')
                require(final.get('supervisor_pid') == child.pid and final.get('launch_finished') is True
                        and final.get('browser_pid', 0) > 0 and final.get('browser_terminated') is True,
                        'browser cleanup unconfirmed')
                require(final.get("browser_ever_front") is False, "fixture was observed foreground before cleanup")
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
