#!/usr/bin/env python3
"""Read-only calibration of the actual supervisor. No browser, display, or input."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time
from macos_observer_calibration import require, validate_record, summarize


def collect(command, seconds, checkpoint, timeout=None):
    """One bounded child, explicit ready/start handshake, preserved partial evidence."""
    records = []
    report = {'records': records, 'completed': False, 'measurement_valid': False}
    child = None
    checkpoint(report)
    try:
        child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, bufsize=0)
        deadline = time.monotonic() + (seconds + 15 if timeout is None else timeout)
        total = 0
        pending = b''
        eof = False
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            while not eof:
                require(time.monotonic() < deadline, 'observer deadline')
                for key, _ in selector.select(min(.1, max(0, deadline - time.monotonic()))):
                    data = os.read(key.fd, 16384)
                    if not data:
                        eof = True
                        break
                    total += len(data)
                    require(total <= 2 * 1024 * 1024, 'observer output limit')
                    pending += data
                    while b'\n' in pending:
                        raw, pending = pending.split(b'\n', 1)
                        require(len(raw) <= 16384 and len(records) < 304, 'observer line/count limit')
                        row = validate_record(json.loads(raw))
                        if not records:
                            require(row['kind'] == 'ready' and row['seconds'] == seconds
                                    and row['pid'] == child.pid, 'observer identity mismatch')
                        records.append(row)
                        checkpoint(report)
                        if len(records) == 1:
                            # Parent acknowledges readiness; no GUI input is sent.
                            child.stdin.write(b's'); child.stdin.flush()
                    require(len(pending) <= 16384, 'observer unterminated line limit')
            require(not pending, 'observer truncated final record')
        code = child.wait(timeout=max(.1, deadline - time.monotonic()))
        report['exit_code'] = code
        report['summary'] = summarize(records)
        report['completed'] = report['summary']['completed']
        require(code == 0 and report['completed'], 'observer incomplete or failed')
        report['measurement_valid'] = True
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        report['error'] = str(error)[:256]
        if records:
            try:
                report['summary'] = summarize(records)
            except ValueError:
                report['invalid_evidence'] = True
    finally:
        if child is not None:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait(timeout=2)
            child.stdin.close(); child.stdout.close()
            report['observer_reaped'] = child.poll() is not None
        checkpoint(report)
    return report


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--supervisor', type=Path, required=True)
    p.add_argument('--sha256', required=True)
    p.add_argument('--seconds', type=int, choices=range(1, 31), default=20)
    p.add_argument('--report', type=Path, required=True)
    p.add_argument('--allow-readonly-input-observation', action='store_true')
    args = p.parse_args()
    if sys.platform != 'darwin' or not args.allow_readonly_input_observation:
        p.error('macOS and explicit read-only observation opt-in required')
    if not re.fullmatch('[0-9a-f]{64}', args.sha256):
        p.error('exact lowercase SHA-256 required')
    binary = args.supervisor.resolve(strict=True)
    if not binary.is_file() or hashlib.sha256(binary.read_bytes()).hexdigest() != args.sha256:
        p.error('supervisor hash mismatch')
    # Exclusive private checkpoint; never truncate or replace an earlier experiment.
    args.report.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(args.report, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as stream:
        def checkpoint(report):
            stream.seek(0)
            json.dump(dict(report, supervisor_sha256=args.sha256, seconds=args.seconds), stream, indent=2)
            stream.write('\n'); stream.truncate(); stream.flush()
        report = collect([str(binary), '--calibrate-input-observer', str(args.seconds)], args.seconds, checkpoint)
    print(json.dumps({k: v for k, v in report.items() if k != 'records'}, indent=2))
    return 0 if report['measurement_valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
