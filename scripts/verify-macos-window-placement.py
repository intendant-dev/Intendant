#!/usr/bin/env python3
"""Opt-in acceptance on an EXISTING owned monitor; never builds or starts a daemon.

Use an owner shell, a supervisor-built controller/fixture, and an exact selector
from list_macos_monitors. Only the disposable fixture is bound/moved/closed.
No permission prompts, credentials changes, input, focus restoration or activation.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import selectors
import subprocess
import tempfile
import time


def require(condition, detail):
    if not condition:
        raise RuntimeError(detail)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bin', required=True, type=Path)
    p.add_argument('--fixture', required=True, type=Path)
    p.add_argument('--port', required=True, type=int)
    p.add_argument('--monitor', required=True)
    p.add_argument('--report', required=True, type=Path)
    p.add_argument('--allow-fixture-window-placement', action='store_true')
    args = p.parse_args()
    if not args.allow_fixture_window_placement or platform.system() != 'Darwin':
        p.error('requires macOS and --allow-fixture-window-placement')
    if not 1 <= args.port <= 65535 or not args.monitor.startswith('macos_virtual:'):
        p.error('requires an explicit port and exact owned monitor selector')
    # Do not quietly promote an agent-session credential to the owner lane.
    if os.getenv('INTENDANT_MCP_URL'):
        p.error('run manually from an owner shell, outside a supervised agent session')
    binary = args.bin.resolve(strict=True)
    fixture_binary = args.fixture.resolve(strict=True)
    report = {'passed': False, 'monitor': args.monitor, 'checks': {}, 'cleanup': {}}
    bindings = []
    child = None
    overall = time.monotonic() + 75

    def call(tool, **arguments):
        require(time.monotonic() < overall, 'acceptance deadline exceeded')
        # Existing ctl authentication and IAM only. Never reads/prints tokens in
        # this harness, changes grants, or retries an uncertain mutation.
        cmd = [str(binary), 'ctl', '--port', str(args.port), '--json', 'tools',
               'call', tool, '--args', json.dumps(arguments)]
        process = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        output = bytearray()
        total = 0
        deadline = time.monotonic() + 25
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(process.stdout, selectors.EVENT_READ, True)
                ready.register(process.stderr, selectors.EVENT_READ, False)
                while ready.get_map():
                    require(time.monotonic() < deadline, f'{tool}: ctl deadline exceeded; effect may be unconfirmed')
                    for key, _ in ready.select(timeout=.1):
                        data = os.read(key.fileobj.fileno(), 4096)
                        if not data:
                            ready.unregister(key.fileobj)
                            continue
                        total += len(data)
                        require(total <= 65536, 'ctl output exceeded cap')
                        if key.data:
                            output.extend(data)
            code = process.wait(timeout=2)
            require(code == 0, f'{tool}: ctl failed (details omitted; inspect owner CLI)')
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=2)
            process.stdout.close()
            process.stderr.close()
        raw = bytes(output)
        return json.loads(raw)

    try:
        monitors = call('list_macos_monitors')['monitors']
        selected = [m for m in monitors if m['display_target'] == args.monitor]
        require(len(selected) == 1, 'selector is not a committed owned monitor')
        monitor = selected[0]
        require(monitor['width'] >= 640 and monitor['height'] >= 480, 'fixture requires at least 640x480')
        with tempfile.TemporaryDirectory(prefix='intendant-window-placement-') as root:
            statuspath = Path(root) / 'fixture.json'
            child = subprocess.Popen([str(fixture_binary), '--disposable-placement-fixture', str(statuspath)],
                                     stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

            def status(predicate=lambda s: True):
                deadline = min(overall, time.monotonic() + 4)
                while time.monotonic() < deadline:
                    require(child.poll() is None, 'disposable fixture exited')
                    if statuspath.exists():
                        require(statuspath.stat().st_size <= 2048, 'fixture output exceeded cap')
                        data = json.loads(statuspath.read_text())
                        if predicate(data):
                            require(data['pid'] == child.pid, 'fixture identity mismatch')
                            return data
                    time.sleep(.05)
                raise RuntimeError('fixture observation deadline exceeded')

            def listed(window_id):
                inventory = call('list_macos_windows', pid=child.pid)
                require(inventory.get('ok'), str(inventory))
                found = [c for c in inventory['candidates'] if c['identity']['window_id'] == window_id]
                require(len(found) == 1, 'fixture window not uniquely listed')
                item = found[0]
                require(item['identity']['pid'] == child.pid and item['identity']['start_seconds'] > 0,
                        'missing PID generation')
                require(isinstance(item.get('candidate'), str) and item['candidate'].startswith('macos_candidate:'),
                        'missing retained candidate token')
                return item

            def bind(item):
                return call('bind_macos_window', display_target=args.monitor,
                            identity=item['identity'], candidate=item['candidate'])

            def refused_bind(item, detail):
                response = bind(item)
                # Account for unexpected success before failing acceptance.
                if 'bound_window' in response:
                    bindings.append(response['bound_window']['binding'])
                require(response.get('ok') is False, detail)
                return response

            initial = status()
            old = listed(initial['window_id'])
            current = listed(initial['window_id'])
            require(old['candidate'] != current['candidate'], 'refresh reused a candidate token')
            report['checks']['inventory_refresh_invalidates_token'] = refused_bind(old, 'obsolete inventory token accepted')
            require(status()['bounds'] == initial['bounds'], 'refused bind moved fixture')
            # Replace only our own listed panel before binding. Native ID reuse
            # is nondeterministic; the hermetic regression forces exact reuse.
            child.stdin.write(b'r'); child.stdin.flush()
            replacement = status(lambda s: s['fixture_generation'] == initial['fixture_generation'] + 1)
            require(replacement['replaced_window_id'] == initial['window_id'], 'wrong fixture was replaced')
            report['checks']['listed_then_replaced_refused'] = refused_bind(current, 'replaced listed AX object accepted')
            require(status()['bounds'] == replacement['bounds'], 'replacement moved during refused bind')
            initial = replacement
            current = listed(initial['window_id'])
            identity = current['identity']
            bound = bind(current)
            require(bound.get('ok'), str(bound))
            binding = bound['bound_window']['binding']
            bindings.append(binding)
            report['checks']['explicit_identity_binding'] = identity
            refused = call('place_macos_window', binding=binding, bounds={'x': -1, 'y': 0, 'width': 320, 'height': 240})
            require(refused.get('ok') is False, 'off-monitor rectangle was accepted')
            require(status()['bounds'] == initial['bounds'], 'refused request moved fixture')
            report['checks']['outside_refused_without_write'] = True
            requested = {'x': 80, 'y': 80, 'width': 400, 'height': 300}
            placed = call('place_macos_window', binding=binding, bounds=requested)
            report['checks']['placement'] = placed
            require(placed.get('ok') and placed['placement']['status'] == 'verified', 'placement was not verified; inspect partial/focus report')
            target = placed['placement']['requested_global']
            observed = status(lambda s: all(abs(s['bounds'][k] - target[k]) <= 1 for k in target))
            report['checks']['independent_fixture_cg_readback'] = observed['bounds']
            unbound = call('unbind_macos_window', binding=binding)
            require(unbound.get('ok'), str(unbound))
            bindings.remove(binding)
            require(call('place_macos_window', binding=binding, bounds=requested).get('ok') is False, 'stale binding accepted')
            report['checks']['consumed_candidate_refused'] = refused_bind(current, 'consumed candidate accepted')
            current = listed(initial['window_id'])
            bound = bind(current)
            require(bound.get('ok'), str(bound))
            binding = bound['bound_window']['binding']; bindings.append(binding)
            child.stdin.write(b'r'); child.stdin.flush()
            replacement = status(lambda s: s['fixture_generation'] == initial['fixture_generation'] + 1)
            require(call('place_macos_window', binding=binding, bounds=requested).get('ok') is False, 'destroyed/replaced window accepted')
            require(status()['bounds'] == replacement['bounds'], 'replacement window was moved')
            require(call('unbind_macos_window', binding=binding).get('ok'), 'could not release stale binding')
            bindings.remove(binding)
            report['checks']['destroyed_window_refused'] = True
            report['passed'] = True
    except Exception as error:
        report['error'] = str(error)[:2048]
    finally:
        # Only our exact subprocess/window and binding handles are cleaned up.
        # The supplied monitor and installed daemon are never destroyed/restarted.
        overall = time.monotonic() + 30
        for binding in bindings:
            try:
                report['cleanup'][binding] = call('unbind_macos_window', binding=binding)
            except Exception as error:
                report['cleanup'][binding] = {'error': str(error)[:512]}
                report['passed'] = False
        if child is not None:
            if child.stdin:
                try:
                    child.stdin.close()
                except BrokenPipeError:
                    pass # already exited
                # EOF closes only the disposable fixture.
            try:
                child.wait(timeout=4)
            except subprocess.TimeoutExpired:
                child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait(timeout=2)
            report['cleanup']['fixture_reaped'] = child.poll() is not None
        # Exclusive output avoids clobbering an existing acceptance record.
        with args.report.open('x') as output:
            json.dump(report, output, indent=2)
        print(f'placement acceptance passed={report["passed"]}; report={args.report}')
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
