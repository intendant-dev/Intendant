#!/usr/bin/env python3
"""Opt-in routing diagnostics. Cooperative controls never establish external key support."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
from macos_key_routing import assess_routing

spec = importlib.util.spec_from_file_location('key_runner', Path(__file__).with_name('verify-macos-raw-pointer.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--route', choices=('construction','application','psn','window-control'), default='construction')
    parser.add_argument('--allow-disposable-key-routing', action='store_true')
    args = parser.parse_args()
    live = args.route != 'construction'
    if platform.system() != 'Darwin' or (live and not args.allow_disposable_key_routing):
        parser.error('requires macOS and explicit opt-in for live diagnostics')
    fixture = args.fixture.resolve(strict=True)
    if not fixture.is_file() or not os.access(fixture, os.X_OK):
        parser.error('fixture must be an explicitly supplied executable')
    flag = {'construction':'--self-test', 'application':'--allow-disposable-key',
            'psn':'--allow-disposable-key-psn',
            'window-control':'--allow-disposable-key-window-control'}[args.route]
    fd = runner.reserve_report(args.report)
    report = dict(route=args.route, passed=False, production_dispatch_enabled=False)
    exit_code = 1
    try:
        code, native, stderr, pid = runner.run_probe([str(fixture), flag])
        report.update(native=native, stderr=stderr, exit_code=code, supervised_pid=pid)
        if live:
            assessment = assess_routing(native, pid, args.route)
            report.update(assessment)
            report['passed'] = assessment['native_background_delivery_verified'] and code == 0
            # A control can complete its diagnostic while support remains false.
            complete_control = assessment['control_effect_verified'] and assessment['diagnostic_valid'] and code == 0
            report['control_diagnostic_completed'] = bool(complete_control)
            exit_code = 0 if report['passed'] else 1
        else:
            report['passed'] = (code == 0 and isinstance(native, dict) and native.get('ok') is True
                and native.get('mode') == 'construction_only'
                and type(native.get('posted_events')) is int and native['posted_events'] == 0
                and native.get('application_created') is False)
            exit_code = 0 if report['passed'] else 1
    except Exception as error:
        report['error'] = str(error)
    finally:
        with os.fdopen(fd, 'w') as output:
            json.dump(report, output, indent=2, allow_nan=False)
    print(json.dumps({key:report.get(key) for key in ('route','passed','diagnostic_valid',
        'control_effect_verified','native_background_delivery_verified','routing_stop')}))
    return exit_code


if __name__ == '__main__':
    raise SystemExit(main())
