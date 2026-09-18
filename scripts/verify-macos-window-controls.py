#!/usr/bin/env python3
"""Opt-in ctl acceptance on an existing disposable owned monitor.

Companion to verify-macos-monitor-http.py, which provides a separate temp-HOME
HTTP daemon and owns monitor cleanup. Never builds, grants permissions, changes
focus or starts an installed daemon. Operates only on its own controls fixture.
"""
import argparse
import contextlib
import json
import os
from pathlib import Path
import platform
import selectors
import subprocess
import tempfile
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bin', required=True, type=Path)
    p.add_argument('--fixture', required=True, type=Path)
    p.add_argument('--port', required=True, type=int)
    p.add_argument('--monitor', required=True)
    p.add_argument('--report', required=True, type=Path)
    p.add_argument('--allow-fixture-window-controls', action='store_true')
    args = p.parse_args()
    if platform.system() != 'Darwin' or not args.allow_fixture_window_controls:
        p.error('requires macOS and --allow-fixture-window-controls')
    if os.getenv('INTENDANT_MCP_URL'):
        p.error('run from the supervisor owner shell, outside a supervised agent session')
    if not 1 <= args.port <= 65535 or not args.monitor.startswith('macos_virtual:'):
        p.error('requires explicit owner port and exact owned monitor selector')
    binary, fixture = args.bin.resolve(strict=True), args.fixture.resolve(strict=True)
    report = {'passed': False, 'monitor': args.monitor, 'checks': {}, 'cleanup': {}}
    rig = tempfile.TemporaryDirectory(prefix="intendant-controls-")
    binding = None
    child = None
    overall = time.monotonic() + 100

    def call(tool, **arguments):
        require(time.monotonic() < overall, 'acceptance deadline exceeded')
        process = subprocess.Popen([str(binary), 'ctl', '--port', str(args.port), '--json',
                                    'tools', 'call', tool, '--args', json.dumps(arguments)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        output = bytearray()
        total = 0
        deadline = min(overall, time.monotonic() + 25)
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(process.stdout, selectors.EVENT_READ, True)
                ready.register(process.stderr, selectors.EVENT_READ, False)
                while ready.get_map():
                    require(time.monotonic() < deadline, 'ctl timeout; effects may be unconfirmed, no retry')
                    for key, _ in ready.select(timeout=.1):
                        data = os.read(key.fileobj.fileno(), 4096)
                        if not data:
                            ready.unregister(key.fileobj)
                            continue
                        total += len(data)
                        require(total <= 65536, 'ctl output limit exceeded')
                        if key.data:
                            output.extend(data)
            require(process.wait(timeout=2) == 0, 'ctl failed; no mutation retry')
            return json.loads(bytes(output))
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=2)
            process.stdout.close()
            process.stderr.close()

    try:
        monitors = call('list_macos_monitors')['monitors']
        require(len([m for m in monitors if m['display_target'] == args.monitor]) == 1,
                'monitor must already be owned by the isolated test daemon')
        with contextlib.nullcontext(rig.name) as root:
            path = Path(root) / 'fixture.json'
            child = subprocess.Popen([str(fixture), '--disposable-controls-fixture', str(path)],
                                     stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

            def status(predicate=lambda s: True):
                deadline = min(overall, time.monotonic() + 4)
                while time.monotonic() < deadline:
                    require(child.poll() is None, 'fixture exited')
                    if path.exists():
                        require(path.stat().st_size <= 8192, 'fixture output limit exceeded')
                        data = json.loads(path.read_text())
                        require(data['pid'] == child.pid, 'fixture PID mismatch')
                        if predicate(data):
                            return data
                    time.sleep(.05)
                raise RuntimeError('independent fixture readback timeout')

            initial = status()
            report["fixture_profile"] = {k: initial.get(k) for k in ("standard_appkit", "field_subrole_present")}
            listed = call('list_macos_windows', pid=child.pid)
            report['checks']['window_listing'] = listed
            candidates = [c for c in listed.get('candidates', []) if c['identity']['window_id'] == initial['window_id']]
            require(len(candidates) == 1, 'own disposable window not uniquely listed')
            bound = call('bind_macos_window', display_target=args.monitor, **{k: candidates[0][k] for k in ('candidate', 'identity')})
            if 'bound_window' in bound:
                binding = bound['bound_window']['binding']
            require(bound.get('ok') is True and binding, str(bound))
            placed = call('place_macos_window', binding=binding, bounds={'x': 60, 'y': 60, 'width': 420, 'height': 340})
            report['checks']['placement'] = placed
            require(placed.get('ok') is True, str(placed))
            requested = placed['placement']['requested_global']
            status(lambda s: all(abs(s['bounds'][k] - requested[k]) <= 1 for k in requested))

            def read():
                # Exercise facade through actual ctl -> HTTP -> IAM dispatch.
                result = call('inspect', argv=['display', 'window-elements', binding])
                require(result.get('ok') is True, str(result))
                controls = result['controls']
                require(0 < len(controls) <= 16, 'unexpected controls inventory')
                require(all(set(c) == {'element', 'role', 'label', 'bounds', 'operations'} for c in controls), 'unexpected fields/text')
                wire = json.dumps(controls)
                require('synthetic-secret' not in wire and 'Omit secure' not in wire and 'Omit disabled' not in wire and 'Omit protected' not in wire and 'synthetic-protected' not in wire, 'secure or disabled control leaked')
                field = [c for c in controls if c['role'] == 'AXTextField' and c['label'] == 'Normal fixture field']
                button = [c for c in controls if c['role'] == 'AXButton' and c['label'] == 'Increment fixture counter']
                require(len(field) == len(button) == 1, 'fixture controls not uniquely described')
                return field[0]['element'], button[0]['element']

            def action(element, value):
                return call('act', argv=['display', 'window-element', binding, element, json.dumps(value)])

            if initial.get('standard_appkit'):
                read()  # Initially protected: the subtree must be absent.
                child.stdin.write(b'u'); child.stdin.flush()
                status(lambda s: s.get('protected_container') is False)
                visible = call('read_macos_window_elements', binding=binding)
                require(visible.get('ok') is True, str(visible))
                exposed = [c for c in visible['controls'] if c['label'] == 'Omit protected fixture']
                require(len(exposed) == 1, 'protected fixture was not visible when explicitly unprotected')
                child.stdin.write(b'p'); child.stdin.flush()
                status(lambda s: s.get('protected_container') is True)
                refused = action(exposed[0]['element'], {'type': 'set_value', 'text': 'must not write'})
                require(refused.get('ok') is False and refused.get('action_attempted') is False,
                        'an element in a newly protected subtree was acted on')
                read()  # Protected again: no label or value may escape.
                report['checks']['protected_subtree_exclusion_and_revalidation'] = True

            old, _ = read()
            field, _ = read()
            rejected = action(old, {'type': 'set_value', 'text': 'must not write'})
            require(rejected.get('ok') is False and rejected.get('action_attempted') is False, str(rejected))
            report['checks']['refresh_rejected'] = rejected
            require(status()['normal_text'] == initial['normal_text'], 'stale token wrote text')
            field, _ = read()
            text = 'Disposable exact text π 😀'
            result = action(field, {'type': 'set_value', 'text': text})
            report['checks']['text_action'] = result
            require(result.get('ok') is True and result['action']['status'] == 'verified', str(result))
            require(text not in json.dumps(result), 'action returned text contents')
            status(lambda s: s['normal_text'] == text)
            replay = action(field, {'type': 'set_value', 'text': 'replay must not write'})
            require(replay.get('ok') is False and replay.get('action_attempted') is False, str(replay))
            require(status()['normal_text'] == text, 'replayed write applied')
            report['checks']['text_independent_and_replay'] = True

            _, button = read()
            count = status()['button_count']
            result = action(button, {'type': 'press'})
            report['checks']['press_action'] = result
            require(result.get('ok') is True and result['action']['status'] == 'dispatched'
                    and result['effects_unconfirmed'] is True, str(result))
            status(lambda s: s['button_count'] == count + 1)
            replay = action(button, {'type': 'press'})
            require(replay.get('ok') is False and replay.get('action_attempted') is False, str(replay))
            time.sleep(.2)  # observation only, never retries an action
            require(status()['button_count'] == count + 1, 'button applied more than once')
            report['checks']['button_exactly_once'] = True

            field, _ = read()
            generation = status()['control_generation']
            child.stdin.write(b'r'); child.stdin.flush()
            replacement = status(lambda s: s['control_generation'] == generation + 1)
            stale = action(field, {'type': 'set_value', 'text': 'must not target replacement'})
            require(stale.get('ok') is False and stale.get('action_attempted') is False, str(stale))
            require(status()['normal_text'] == replacement['normal_text'], 'old AX object substituted replacement')
            report['checks']['replacement_rejected'] = stale

            field, _ = read()
            require(call('unbind_macos_window', binding=binding).get('ok') is True, 'unbind failed')
            stale = action(field, {'type': 'set_value', 'text': 'must not write after unbind'})
            require(stale.get('ok') is False and stale.get('action_attempted') is False, str(stale))
            binding = None
            report['checks']['unbind_rejected'] = True
            report['passed'] = True
    except Exception as exc:
        report['error'] = str(exc)
    finally:
        # Give exact cleanup a separate bound even when the exercise timed out.
        overall = time.monotonic() + 30
        if binding:
            try:
                result = call('unbind_macos_window', binding=binding)
                report['cleanup']['unbind'] = result
                require(result.get('ok') is True, 'unbind cleanup unconfirmed')
            except Exception as exc:
                report['cleanup']['error'] = str(exc)
                report['passed'] = False
        if child:
            if child.poll() is None:
                try:
                    child.stdin.write(b'q'); child.stdin.flush(); child.stdin.close()
                    child.wait(timeout=4)
                except (OSError, subprocess.TimeoutExpired):
                    child.terminate()
                    try:
                        child.wait(timeout=4)
                    except subprocess.TimeoutExpired:
                        child.kill(); child.wait(timeout=4)
            report['cleanup']['fixture_reaped'] = child.poll() is not None
        # Reap the writer before removing its directory; never mask the original
        # action failure with a concurrent TemporaryDirectory removal error.
        try:
            rig.cleanup()
            report['cleanup']['directory_removed'] = True
        except OSError as exc:
            report['cleanup']['directory_error'] = str(exc)
            report['passed'] = False
        args.report.write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
