#!/usr/bin/env python3
"""Read-only calibration of the actual supervisor. No browser, display, or input."""
import argparse
import contextlib
import stat
import tempfile
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


# Keep verification and execution on the same private byte snapshot.
MAX_SUPERVISOR_BYTES = 64 * 1024 * 1024


@contextlib.contextmanager
def verified_supervisor(source, expected_hash):
    """Prevent build-path replacement; not a sandbox against same-user attackers.

    Access observations still come from the actual executing observer process.
    The caller keeps this context alive until its owned child is reaped.
    """
    require(re.fullmatch('[0-9a-f]{64}', expected_hash) is not None,
            'exact lowercase supervisor SHA-256 required')
    flags = os.O_RDONLY | getattr(os, 'O_CLOEXEC', 0) | getattr(os, 'O_NONBLOCK', 0)
    flags |= getattr(os, 'O_NOFOLLOW', 0) | getattr(os, 'O_BINARY', 0)
    descriptor = os.open(Path(source).resolve(strict=True), flags)
    try:
        info = os.fstat(descriptor)
        require(stat.S_ISREG(info.st_mode) and info.st_mode & 0o111,
                'supervisor must be a regular executable file')
        require(0 < info.st_size <= MAX_SUPERVISOR_BYTES, 'supervisor size limit')
        with os.fdopen(descriptor, 'rb', closefd=False) as stream:
            image = stream.read(MAX_SUPERVISOR_BYTES + 1)
    finally:
        os.close(descriptor)
    require(0 < len(image) <= MAX_SUPERVISOR_BYTES, 'supervisor size limit')
    require(hashlib.sha256(image).hexdigest() == expected_hash, 'supervisor hash mismatch')
    with tempfile.TemporaryDirectory(prefix='intendant-observer-verified-') as directory:
        executable = Path(directory) / 'browser-supervisor'
        with executable.open('xb') as output:
            output.write(image)
        executable.chmod(0o500)
        require(hashlib.sha256(executable.read_bytes()).hexdigest() == expected_hash,
                'private supervisor hash mismatch')
        yield executable


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
    # Keep the verified copy alive through owned-observer cleanup.
    with verified_supervisor(args.supervisor, args.sha256) as binary:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(args.report, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as stream:
            def checkpoint(report):
                stream.seek(0)
                json.dump(dict(report, supervisor_sha256=args.sha256, seconds=args.seconds,
                               supervisor_execution='private_verified_copy'), stream, indent=2)
                stream.write(chr(10)); stream.truncate(); stream.flush()
            report = collect([str(binary), '--calibrate-input-observer', str(args.seconds)], args.seconds, checkpoint)
    print(json.dumps({k: v for k, v in report.items() if k != 'records'}, indent=2))
    return 0 if report['measurement_valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
