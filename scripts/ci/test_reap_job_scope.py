#!/usr/bin/env python3
"""Tests for reap_job_scope.py: python3 -m unittest scripts/ci/test_reap_job_scope.py"""
import contextlib
import importlib.util
import io
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('reap_job_scope', Path(__file__).with_name('reap_job_scope.py'))
reaper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reaper)

def run_main(argv):
    """main() with its output captured: the ::warning:: lines it prints
    would otherwise surface as annotations on every CI run."""
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = reaper.main(argv)
    return code, out.getvalue()


RUNNER_TEMP = '/srv/runner/_work/_temp'
SCOPE = RUNNER_TEMP + '/tmp'
UID = 1001


class FakeProc:
    """A /proc look-alike: stat, status, cmdline, environ and a cwd link."""

    def __init__(self, root):
        self.root = Path(root)

    def add(self, pid, ppid, argv, env=(), cwd='/', uid=UID, start=None, state='S', comm=None):
        entry = self.root / str(pid)
        entry.mkdir()
        comm = comm or Path(argv[0]).name[:15]
        fields = [state, str(ppid)] + ['0'] * 17 + [str(start if start is not None else pid * 10), '0']
        (entry / 'stat').write_text(f'{pid} ({comm}) ' + ' '.join(fields) + '\n')
        (entry / 'status').write_text(f'Name:\t{comm}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n')
        (entry / 'cmdline').write_bytes(b'\0'.join(os.fsencode(a) for a in argv) + b'\0')
        (entry / 'environ').write_bytes(b''.join(os.fsencode(e) + b'\0' for e in env))
        os.symlink(cwd, entry / 'cwd')


class FindResidueTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.proc = FakeProc(self.temp.name)
        # init <- runner service <- Listener <- Worker <- step shell <- janitor
        self.proc.add(1, 0, ['/sbin/init'], uid=0)
        self.proc.add(10, 1, ['/bin/bash', '/srv/runner/runsvc.sh'])
        self.proc.add(11, 10, ['/srv/runner/bin/Runner.Listener', 'run'])
        self.proc.add(12, 11, ['/srv/runner/bin/Runner.Worker', 'spawnclient'],
                      env=[f'TMPDIR={SCOPE}'])
        self.proc.add(20, 12, ['/bin/bash', '-e', f'{RUNNER_TEMP}/step.sh'], env=[f'TMPDIR={SCOPE}'])
        self.proc.add(21, 20, [f'{RUNNER_TEMP}/pyws/bin/python3', 'scripts/ci/reap_job_scope.py'],
                      env=[f'TMPDIR={SCOPE}'])

    def tearDown(self):
        self.temp.cleanup()

    def residue(self):
        return {pid: reason for pid, _, _, reason in
                reaper.find_residue(Path(self.temp.name), SCOPE, UID, 21)}

    def test_finds_orphans_by_cwd_argv_and_env(self):
        # A scrubbed-env acceptance daemon: only HOME and cwd point in.
        self.proc.add(30, 1, ['/cache/target/debug/intendant', '--web', '0'],
                      env=[f'HOME={SCOPE}/intendant-external-cu-x/home', 'LANG=C.UTF-8'],
                      cwd=f'{SCOPE}/intendant-external-cu-x')
        # Its browser: a profile dir in argv.
        self.proc.add(31, 30, ['/usr/lib/chromium/chromium', f'--user-data-dir={SCOPE}/p'])
        # An e2e daemon orphaned by a cancelled job: the inherited TMPDIR.
        self.proc.add(32, 1, ['/cache/target/debug/intendant', '--takeover'],
                      env=['HOME=/elsewhere', f'TMPDIR={SCOPE}'])
        self.assertEqual(self.residue(), {30: 'cwd', 31: 'argv', 32: 'env TMPDIR'})

    def test_spares_self_ancestors_children_and_runner_stack(self):
        self.proc.add(22, 21, ['/bin/sleep', '1'], env=[f'TMPDIR={SCOPE}/tmp'])
        self.proc.add(40, 1, ['/usr/bin/sccache'], env=[f'TMPDIR={SCOPE}/tmp'])
        self.proc.add(41, 1, ['/bin/sh'], env=[f'TMPDIR={SCOPE}/tmp'], state='Z')
        self.assertEqual(self.residue(), {})

    def test_spares_other_accounts_listeners_and_older_residue(self):
        self.proc.add(50, 1, ['/opt/intendant'], env=[f'HOME={SCOPE}/h'], uid=UID + 1)
        other = '/srv/runner-b/_work/_temp/tmp'
        self.proc.add(51, 1, ['/cache/intendant'], env=[f'TMPDIR={other}'], cwd=f'{other}/x')
        self.proc.add(52, 1, ['/cache/intendant'], env=[f'HOME={SCOPE}2/h'], cwd=f'{SCOPE}-old')
        self.proc.add(54, 1, ['/cache/intendant'], env=[f'TMPDIR=/mnt{SCOPE}'], cwd=f'/mnt{SCOPE}/x')
        # Pre-scoping residue: references the runner temp, not the job dir.
        self.proc.add(53, 1, ['/cache/intendant'], env=[f'PATH={RUNNER_TEMP}/pyws/bin:/usr/bin'],
                      cwd='/tmp/intendant-external-cu-y')
        self.assertEqual(self.residue(), {})


class ReferencesTest(unittest.TestCase):
    def test_whole_path_boundaries(self):
        root = SCOPE.encode()
        for hit in [root, root + b'/x', b'TMPDIR=' + root, b'PATH=/bin:' + root + b'/v/bin:/usr/bin',
                    b'--user-data-dir=' + root + b'/p', b"'" + root + b"/q'"]:
            self.assertTrue(reaper.references(hit, root), hit)
        for miss in [root + b'2', root + b'-old/x', b'/mnt' + root, b'TMPDIR=/mnt' + root + b'/x']:
            self.assertFalse(reaper.references(miss, root), miss)

    def test_annotation_escapes_workflow_command_text(self):
        self.assertEqual(reaper.annotation('a%b\r\nc'), 'a%25b%0D%0Ac')


class GuardTest(unittest.TestCase):
    def test_scope_must_be_a_job_dir_inside_a_runner_temp(self):
        self.assertTrue(reaper.valid_scope(SCOPE))
        for bad in ['', '/', 'relative/_work/_temp/tmp', '/_temp/tmp', RUNNER_TEMP, '/srv/tmp',
                    f'{SCOPE}/', '/a/../b/_work/_temp/tmp']:
            self.assertFalse(reaper.valid_scope(bad), bad)

    def test_refused_scope_is_a_no_op(self):
        with tempfile.TemporaryDirectory() as base:
            keep = os.path.join(base, 'keep')
            os.makedirs(keep)
            code, out = run_main(['--scope', base, '--remove'])
            self.assertEqual(code, 0)
            self.assertIn('refusing scope', out)
            self.assertTrue(os.path.isdir(keep))

    def test_remove_deletes_the_job_dir_only(self):
        with tempfile.TemporaryDirectory() as base:
            job = os.path.join(base, '_work', '_temp', 'tmp')
            os.makedirs(os.path.join(job, 'leak', 'deep'))
            open(os.path.join(job, '.tmpfile'), 'w').close()
            self.assertEqual(reaper.remove_scope(job), 2)
            self.assertFalse(os.path.exists(job))
            self.assertTrue(os.path.isdir(os.path.dirname(job)))
            self.assertEqual(reaper.remove_scope(job), 0)


@unittest.skipUnless(sys.platform.startswith('linux'), 'reads the real /proc')
class LiveReapTest(unittest.TestCase):
    def test_reaps_an_orphan_that_ignores_sigterm(self):
        with tempfile.TemporaryDirectory() as base:
            scope = os.path.join(os.path.realpath(base), 'work', '_temp', 'tmp')
            os.makedirs(scope)
            # Double fork: the shell exits at once, the sleeper is orphaned
            # (no longer this test's descendant) and ignores TERM.
            launcher = subprocess.run(
                ['sh', '-c', "trap '' TERM; sleep 60 >/dev/null 2>&1 & echo $!"],
                env={**os.environ, 'TMPDIR': scope},
                capture_output=True, text=True, check=True)
            orphan = int(launcher.stdout.strip())
            try:
                start = reaper.read_stat(Path('/proc'), orphan)[2]
                code, out = run_main(['--scope', scope, '--remove', '--grace', '0.5'])
                self.assertEqual(code, 0)
                self.assertIn(f'stopping leftover process {orphan} (env TMPDIR)', out)
                self.assertFalse(reaper.same_process(Path('/proc'), orphan, start))
                self.assertFalse(os.path.exists(scope))
            finally:
                try:
                    os.kill(orphan, signal.SIGKILL)
                except OSError:
                    pass


if __name__ == '__main__':
    unittest.main()
