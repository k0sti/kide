"""Regression tests for the publication boundary; uses only temporary Git repos."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


AUDIT = Path(__file__).with_name("public_audit.py")


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.org")
        self.commit("clean.txt", b"public\n")
        self.base = self.git("rev-parse", "HEAD").strip()
        policy = Path(self.temp.name) / "policy.json"
        policy.write_text(json.dumps({"public_base": self.base,
                                      "patterns": {"private_name": "not-for-release"}}))
        self.git("config", "kide.auditPolicy", str(policy))

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo,
                                       stderr=subprocess.DEVNULL, text=True, timeout=5)

    def commit(self, name, data, message="update"):
        (self.repo / name).write_bytes(data)
        self.git("add", "--all")
        self.git("commit", "-qm", message)

    def audit(self, *args, stdin=None):
        return subprocess.run([sys.executable, "-I", str(AUDIT), *args],
                              cwd=self.repo, input=stdin, capture_output=True,
                              text=True, timeout=25)

    def test_clean_tree_and_metadata(self):
        self.commit("hello.txt", b"hello\n")
        self.assertEqual(self.audit("--check", "HEAD").returncode, 0)

    def test_missing_private_policy_blocks(self):
        self.git("config", "--unset", "kide.auditPolicy")
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)

    def test_deleted_secret_in_an_earlier_commit_blocks(self):
        self.commit("removed.txt", ("nsec" + "1" + "q" * 59).encode())
        self.git("rm", "-q", "removed.txt")
        self.git("commit", "-qm", "remove")
        result = self.audit("--check", "HEAD")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nostr_private_key", result.stderr)
        self.assertNotIn("q" * 59, result.stderr)

    def test_private_name_in_path_or_commit_message_blocks(self):
        self.commit("not-for-release.txt", b"ordinary text")
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)
        self.git("reset", "--hard", self.base)
        self.commit("ok.txt", b"ordinary text", "not-for-release")
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)

    def test_unrelated_history_blocks(self):
        self.git("checkout", "--orphan", "unrelated")
        self.git("commit", "--allow-empty", "-qm", "unrelated")
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)

    def test_merged_private_history_blocks(self):
        self.git("checkout", "--orphan", "unrelated")
        self.git("commit", "--allow-empty", "-qm", "unrelated")
        other = self.git("rev-parse", "HEAD").strip()
        self.git("checkout", "--detach", self.base)
        self.git("merge", "--allow-unrelated-histories", "--no-edit", other)
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)

    def test_oversize_file_and_symlink_block(self):
        self.commit("big", b"x" * (4 * 1024 * 1024 + 1))
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)
        self.git("reset", "--hard", self.base)
        (self.repo / "link").symlink_to("clean.txt")
        self.git("add", "link")
        self.git("commit", "-qm", "symlink")
        self.assertNotEqual(self.audit("--check", "HEAD").returncode, 0)

    def test_hook_checks_every_ref_and_tag_metadata(self):
        self.commit("good", b"ok")
        good = self.git("rev-parse", "HEAD").strip()
        self.git("tag", "-am", "not-for-release", "bad")
        bad = self.git("rev-parse", "bad").strip()
        lines = f"refs/heads/dev {good} refs/heads/dev {'0' * 40}\n"
        args = ("--pre-push", "origin", "git@github.com:k0sti/kide.git")
        self.assertEqual(self.audit(*args, stdin=lines).returncode, 0)
        lines += f"refs/tags/bad {bad} refs/tags/bad {'0' * 40}\n"
        self.assertNotEqual(self.audit(*args, stdin=lines).returncode, 0)

    def test_hook_refuses_another_destination(self):
        args = ("--pre-push", "origin", "https://example.org/elsewhere")
        self.assertNotEqual(self.audit(*args, stdin="").returncode, 0)

    def test_local_replace_ref_cannot_hide_committed_secret(self):
        self.commit("hidden", ("nsec" + "1" + "q" * 59).encode())
        bad = self.git("rev-parse", "HEAD").strip()
        self.git("replace", bad, self.base)
        result = self.audit("--check", "HEAD")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nostr_private_key", result.stderr)

    def test_tracked_shell_hook_runs_audit(self):
        (self.repo / "scripts").mkdir()
        shutil.copyfile(AUDIT, self.repo / "scripts/public_audit.py")
        self.git("add", "scripts")
        self.git("commit", "-qm", "install audit")
        oid = self.git("rev-parse", "HEAD").strip()
        hook = AUDIT.parent.parent / ".githooks/pre-push"
        result = subprocess.run(["sh", str(hook), "origin", "git@github.com:k0sti/kide.git"],
                                input=f"refs/heads/dev {oid} refs/heads/dev {self.base}\n",
                                cwd=self.repo, text=True, capture_output=True, timeout=25)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
