#!/usr/bin/env python3
"""Nonposting raw-pointer construction check; live self-process probe is opt-in.

No arbitrary PID/window target, no installed daemon, no hotplug, no global input.
A successful construction-only run is NOT a delivery or canvas acceptance pass.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import selectors
import subprocess
import time

from macos_input_evidence import assess_desktop, assess_pointer_receipts


def strict_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError('duplicate native JSON key')
            result[key] = value
        return result
    def constant(_):
        raise ValueError('nonfinite native JSON value')
    return json.loads(data, object_pairs_hook=pairs, parse_constant=constant)


def run_probe(argv, timeout=15):
    """Bound both streams and lifetime, preserving negative native evidence."""
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    streams = [bytearray(), bytearray()]
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as poll:
            poll.register(child.stdout, selectors.EVENT_READ, 0)
            poll.register(child.stderr, selectors.EVENT_READ, 1)
            while poll.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError('native probe deadline; delivery may be unconfirmed')
                for key, _ in poll.select(min(.05, remaining)):
                    data = os.read(key.fileobj.fileno(), 4096)
                    if not data:
                        poll.unregister(key.fileobj)
                        continue
                    output = streams[key.data]
                    output.extend(data)
                    if len(output) > (16384 if key.data == 0 else 4096):
                        raise RuntimeError('native probe output limit')
        code = child.wait(timeout=max(.01, deadline - time.monotonic()))
        return code, strict_json(streams[0]), streams[1].decode('utf-8', errors='replace'), child.pid
    finally:
        if child.poll() is None:
            child.kill()
            child.wait(timeout=5)
        child.stdout.close()
        child.stderr.close()


def assess_native(native, live, supervised_pid=None):
    expected = 'self_process_click' if live else 'construction_only'
    result = {'passed': False, 'mode': expected, 'production_dispatch_enabled': False,
              'cross_process_verified': False}
    if not isinstance(native, dict) or native.get('mode') != expected:
        result['error'] = 'native evidence mode mismatch'
        return result
    posted = native.get('posted_events')
    if not live:
        result['passed'] = (native.get('ok') is True and type(posted) is int and posted == 0
                            and native.get('application_created') is False
                            and native.get('delivery_verified') is False
                            and native.get('effect_verified') is False
                            and type(native.get('native_cases')) is int
                            and native['native_cases'] >= 13)
        result['delivery'] = 'not_exercised'
        result['effect_verified'] = False
        return result
    plan = native.get('plan')
    evidence = assess_pointer_receipts(plan, native.get('receipts'), native.get('tagged_click_count'))
    result['pointer_evidence'] = evidence
    target = plan.get('pid') if isinstance(plan, dict) else None
    result['desktop_observation_assessment'] = assess_desktop(
        native.get('before'), native.get('after'), target, native.get('target_ever_front'))
    result['passed'] = (native.get('ok') is True and type(posted) is int and posted == 2
                        and evidence['effect_verified'] and plan.get('pid') == supervised_pid
                        and plan.get('source_pid') == supervised_pid
                        and native.get('window_closed') is True
                        and native.get('receipt_overflow') is False
                        and native.get('observations_complete') is True
                        and result['desktop_observation_assessment']['target_foreground_observed'] is False)
    return result


def reserve_report(path):
    # Existing files and symlinks must refuse before possible native dispatch.
    return os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--fixture', required=True, type=Path)
    p.add_argument('--report', required=True, type=Path)
    p.add_argument('--allow-disposable-process-click', action='store_true',
                   help='explicitly allow one real click to the probe process itself; never an arbitrary app')
    args = p.parse_args()
    if platform.system() != 'Darwin':
        p.error('native probe requires macOS')
    fixture = args.fixture.resolve(strict=True)
    if not fixture.is_file() or not os.access(fixture, os.X_OK):
        p.error('fixture must be an explicitly supplied executable')
    # Reserve an owner-private, non-overwriting report BEFORE any possible effect.
    fd = reserve_report(args.report)
    report = {'passed': False, 'mode': 'self_process_click' if args.allow_disposable_process_click
              else 'construction_only', 'production_dispatch_enabled': False}
    try:
        flag = '--allow-disposable-process-click' if args.allow_disposable_process_click else '--self-test'
        code, native, stderr, pid = run_probe([str(fixture), flag])
        report.update(assess_native(native, args.allow_disposable_process_click, pid))
        report.update({'exit_code': code, 'native': native, 'native_stderr': stderr, 'supervised_pid': pid})
        report['passed'] = report['passed'] and code == 0
    except Exception as error:
        report['error'] = str(error)
    finally:
        with os.fdopen(fd, 'w') as output:
            json.dump(report, output, indent=2, allow_nan=False)
            output.write('\n')
    print(json.dumps({k: report[k] for k in ('passed', 'mode', 'production_dispatch_enabled')}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
