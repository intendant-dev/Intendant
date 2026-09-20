#!/usr/bin/env python3
"""Default is nonposting construction; live self-process ArrowRight requires opt-in."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
from macos_key_evidence import assess_self, integer

spec = importlib.util.spec_from_file_location('raw_runner', Path(__file__).with_name('verify-macos-raw-pointer.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture', required=True, type=Path)
    parser.add_argument('--report', required=True, type=Path)
    parser.add_argument('--allow-disposable-key', action='store_true')
    args = parser.parse_args()
    if platform.system() != 'Darwin':
        parser.error('native fixture requires macOS')
    fixture = args.fixture.resolve(strict=True)
    if not fixture.is_file() or not os.access(fixture, os.X_OK):
        parser.error('fixture must be an explicitly supplied executable')
    fd = runner.reserve_report(args.report)
    report = {'passed': False, 'production_dispatch_enabled': False}
    try:
        flag = '--allow-disposable-key' if args.allow_disposable_key else '--self-test'
        code, native, stderr, pid = runner.run_probe([str(fixture), flag])
        report.update(native=native, stderr=stderr, exit_code=code, supervised_pid=pid)
        if args.allow_disposable_key:
            report.update(assess_self(native, pid))
        else:
            report['mode'] = 'construction_only'
            report['passed'] = (isinstance(native, dict) and native.get('mode') == report['mode']
                and native.get('ok') is True and native.get('application_created') is False
                and integer(native.get('posted_events'), 0, 0) and integer(native.get('cases'), 7, 100))
        report['passed'] = report['passed'] and code == 0
    except Exception as error:
        report['error'] = str(error)
    finally:
        with os.fdopen(fd, 'w') as output:
            json.dump(report, output, indent=2, allow_nan=False)
            output.write('\n')
    print(json.dumps({k: report.get(k) for k in ('passed', 'queue_delivery_verified', 'receiver_delivery_verified', 'effect_verified')}))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
