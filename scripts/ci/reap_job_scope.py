#!/usr/bin/env python3
"""Job-scope janitor for the self-hosted Linux CI leg (windows.yml).

The Linux fleet leg points TMPDIR at a job-scoped directory,
$RUNNER_TEMP/tmp ("Job-scoped temp dir" step), so every later step's
processes inherit it and every tempfile/mkdtemp/os.tmpdir() root lands in
it. This script runs as the job's last, `always()` step and:

  1. stops every process of this account that still references the job
     temp dir -- through its working directory, its argv or its
     environment (the inherited TMPDIR, a HOME or profile dir minted under
     it) -- except itself, its ancestors (the step shell, Runner.Worker),
     its own children and the runner service stack: TERM, a short grace,
     then KILL;
  2. with --remove, deletes the job temp dir.

Why a marker instead of the process tree: when a job is cancelled, the
runner's tree kill SIGKILLs test binaries before their Drop-based
`kill_on_drop` cleanup can run, so the test daemons they spawned are
reparented to init and survive the job; acceptance scripts also launch
daemons with a scrubbed environment, which hides them from any
RUNNER_TRACKING_ID-based sweep. Every such process still references the
job temp dir. It lives under the listener's own $RUNNER_TEMP and a
listener runs one job at a time, so the marker never matches the other
listener's live job.

Always exits 0: janitorial trouble must never fail CI. Each reaped process
is reported as a ::warning:: so leaks stay visible on the run.
"""
import argparse
import os
import re
import shutil
import signal
import sys
import time
from pathlib import Path

# Never signalled, whatever they reference: the runner service stack and a
# shared compiler cache server (killing it mid-compile fails the other
# listener's rustc invocations). Mirrors scripts/ci/hooks/hook-lib.sh.
PROTECTED_MARKERS = (b'runsvc.sh', b'RunnerService.js', b'Runner.Listener',
                     b'Runner.Worker', b'sccache')


def valid_scope(scope):
    """Accept only an absolute job temp dir inside a runner temp dir
    (<work>/_temp/<name>).

    An empty or shallow scope would match nearly every process of the
    account, so anything else refuses the whole run.
    """
    path = Path(scope)
    return (path.is_absolute() and os.path.normpath(scope) == scope
            and path.parent.name == '_temp' and len(path.parts) >= 5)


def references(value, root):
    """True when `value` mentions `root` as a path (or a PATH entry)."""
    return re.search(re.escape(root) + rb'(?=/|:|$)', value) is not None


def read_stat(proc_root, pid):
    """(state, ppid, starttime) from /proc/<pid>/stat, or None if gone."""
    try:
        stat = (proc_root / str(pid) / 'stat').read_bytes()
    except OSError:
        return None
    # comm may contain spaces and ')': the fields resume after the last ')'.
    fields = stat[stat.rfind(b')') + 2:].split()
    try:
        return fields[0], int(fields[1]), int(fields[19])
    except (IndexError, ValueError):
        return None


def real_uid(proc_root, pid):
    try:
        for line in (proc_root / str(pid) / 'status').read_bytes().splitlines():
            if line.startswith(b'Uid:'):
                return int(line.split()[1])
    except (OSError, IndexError, ValueError):
        pass
    return None


def read_list(proc_root, pid, name):
    try:
        return [item for item in (proc_root / str(pid) / name).read_bytes().split(b'\0') if item]
    except OSError:
        return []


def scope_reason(proc_root, pid, root):
    """Why `pid` belongs to the job scope, or None."""
    try:
        cwd = os.fsencode(os.readlink(proc_root / str(pid) / 'cwd'))
    except OSError:
        cwd = b''
    if references(cwd, root):
        return 'cwd'
    if any(references(arg, root) for arg in read_list(proc_root, pid, 'cmdline')):
        return 'argv'
    for entry in read_list(proc_root, pid, 'environ'):
        if references(entry, root):
            return 'env ' + entry.split(b'=', 1)[0].decode(errors='replace')
    return None


def find_residue(proc_root, scope, uid, self_pid):
    """[(pid, starttime, command, reason)] for this account's job residue."""
    root = os.fsencode(scope)
    snapshot = {}
    for entry in proc_root.iterdir():
        if entry.name.isdigit():
            stat = read_stat(proc_root, int(entry.name))
            if stat is not None:
                snapshot[int(entry.name)] = stat
    protected = set()
    pid = self_pid
    while pid > 0 and pid not in protected:
        protected.add(pid)
        pid = snapshot[pid][1] if pid in snapshot else 0
    # This script's own children (none today) are part of the step, too.
    own_tree = {self_pid}
    changed = True
    while changed:
        changed = False
        for pid, (_, ppid, _) in snapshot.items():
            if ppid in own_tree and pid not in own_tree:
                own_tree.add(pid)
                changed = True
    residue = []
    for pid, (state, _, start) in sorted(snapshot.items()):
        if pid in protected or pid in own_tree or state == b'Z':
            continue
        if real_uid(proc_root, pid) != uid:
            continue
        argv = read_list(proc_root, pid, 'cmdline')
        if any(marker in arg for arg in argv for marker in PROTECTED_MARKERS):
            continue
        reason = scope_reason(proc_root, pid, root)
        if reason:
            command = b' '.join(argv)[:160].decode(errors='replace') or '?'
            residue.append((pid, start, command, reason))
    return residue


def same_process(proc_root, pid, start):
    stat = read_stat(proc_root, pid)
    return stat is not None and stat[2] == start and stat[0] != b'Z'


def reap(proc_root, residue, grace, kill=os.kill):
    """TERM, wait up to `grace` seconds, KILL; returns pids that survived."""
    live = []
    for pid, start, _, _ in residue:
        # Re-check identity right before signalling: never hit a recycled pid.
        if same_process(proc_root, pid, start):
            try:
                kill(pid, signal.SIGTERM)
                live.append((pid, start))
            except OSError:
                pass
    deadline = time.monotonic() + grace
    while live and time.monotonic() < deadline:
        time.sleep(0.2)
        live = [(pid, start) for pid, start in live if same_process(proc_root, pid, start)]
    for pid, start in live:
        if same_process(proc_root, pid, start):
            try:
                kill(pid, signal.SIGKILL)
            except OSError:
                pass
    time.sleep(0.5 if live else 0)
    return [pid for pid, start in live if same_process(proc_root, pid, start)]


def remove_scope(scope):
    """Delete the job temp dir; returns how many entries it held."""
    target = Path(scope)
    if not target.is_dir():
        return 0
    entries = sum(1 for _ in target.iterdir())
    shutil.rmtree(target, ignore_errors=True)
    return entries


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--scope', required=True, help='the job temp dir ($RUNNER_TEMP/tmp)')
    parser.add_argument('--remove', action='store_true', help='delete the job temp dir afterwards')
    parser.add_argument('--grace', type=float, default=5.0, help='seconds between TERM and KILL')
    parser.add_argument('--proc-root', type=Path, default=Path('/proc'), help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if not valid_scope(args.scope):
        print(f'::warning::job-scope janitor: refusing scope {args.scope!r}')
        return 0
    try:
        residue = find_residue(args.proc_root, args.scope, os.getuid(), os.getpid())
        for pid, _, command, reason in residue:
            print(f'::warning::job-scope janitor: stopping leftover process {pid} ({reason}): {command}')
        survivors = reap(args.proc_root, residue, args.grace) if residue else []
        for pid in survivors:
            print(f'::warning::job-scope janitor: process {pid} survived SIGKILL')
        removed = remove_scope(args.scope) if args.remove else 0
        print(f'job-scope janitor: stopped {len(residue) - len(survivors)} leftover process(es); '
              f'removed {removed} temp entr{"y" if removed == 1 else "ies"}')
    except Exception as error:  # noqa: BLE001 -- janitorial: report, never fail the job
        print(f'::warning::job-scope janitor failed: {error}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
