#!/usr/bin/env python3
"""Bounded audit of committed publication inputs, never the working directory.

Private patterns stay in a local policy file configured by kide.auditPolicy.
Limits match the initial public-tree audit; exceeding a limit is not a pass.
"""

import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import subprocess
import sys


SECONDS = 20
FILES = 3000
TOTAL_BYTES = 64 * 1024 * 1024
FILE_BYTES = 4 * 1024 * 1024
COMMITS = 300
PUBLIC_URLS = {"git@github.com:k0sti/kide.git", "git@github.com:k0sti/kide",
               "https://github.com/k0sti/kide.git", "https://github.com/k0sti/kide"}
PATTERNS = {
    "private_key_pem": rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----",
    "access_token": rb"\b(?:AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{30,}|"
                    rb"github_pat_[A-Za-z0-9_]{40,}|xox[baprs]-[A-Za-z0-9-]{20,})\b",
    "nostr_private_key": rb"\bnsec1[023456789acdefghjklmnpqrstuvwxyz]{50,}\b",
    "credential_url": rb"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s/:@]+:[^\s/@]+@",
}
FORBIDDEN = {".env", "device.key", "tls.key", "id_rsa", "id_ed25519"}


class AuditError(Exception):
    pass


def expired(_signum, _frame):
    raise AuditError("20-second audit deadline exceeded")


def git(*args, limit=FILE_BYTES):
    # Bounded reads, with a process-wide deadline even if a child blocks on I/O.
    environment = dict(os.environ, GIT_NO_REPLACE_OBJECTS="1")
    process = subprocess.Popen(["git", *args], stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, env=environment)
    try:
        output = process.stdout.read(limit + 1)
        if len(output) > limit:
            raise AuditError("Git output exceeded audit byte bound")
        if process.wait() != 0:
            raise AuditError("Git inspection failed; no audit pass")
        return output
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        process.stdout.close()


class Audit:
    def __init__(self):
        try:
            configured = git("config", "--get", "kide.auditPolicy", limit=4096).strip()
        except AuditError:
            raise AuditError("configure kide.auditPolicy with a private local policy file") from None
        policy_path = Path(configured.decode())
        if not policy_path.is_absolute():
            raise AuditError("audit policy must be outside the working tree")
        root = Path(git("rev-parse", "--show-toplevel").decode().strip()).resolve()
        if policy_path.resolve().is_relative_to(root) and ".git" not in policy_path.parts:
            raise AuditError("audit policy must not be publishable working-tree data")
        with policy_path.open("rb") as source:
            raw = source.read(65537)
        if len(raw) > 65536:
            raise AuditError("private policy exceeded byte bound")
        policy = json.loads(raw)
        self.base = policy["public_base"]
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", self.base):
            raise AuditError("invalid audited public base")
        git("cat-file", "-e", self.base + "^{commit}")
        if git("rev-parse", "--is-shallow-repository").strip() != b"false":
            raise AuditError("shallow history cannot be audited for publication")
        extra = policy["patterns"]
        if not isinstance(extra, dict) or not extra or len(extra) > 32:
            raise AuditError("private patterns are required (at most 32)")
        self.patterns = [(name, re.compile(pattern)) for name, pattern in PATTERNS.items()]
        for name, pattern in extra.items():
            if len(pattern) > 4096 or not re.fullmatch(r"[a-z_]{1,64}", name):
                raise AuditError("invalid private audit pattern")
            self.patterns.append((name, re.compile(pattern.encode())))
        self.blobs = set()
        self.trees = set()
        self.commits = set()
        self.files = 0
        self.bytes = 0
        self.entries = set()

    def inspect(self, data):
        for name, pattern in self.patterns:
            if pattern.search(data):
                # Never print the matching bytes, including from commit messages.
                raise AuditError("finding: " + name)

    def blob(self, oid):
        if oid in self.blobs:
            return
        data = git("cat-file", "blob", oid)
        self.bytes += len(data)
        if self.bytes > TOTAL_BYTES:
            raise AuditError("64 MiB total audit byte bound exceeded")
        self.inspect(data)
        self.blobs.add(oid)

    def tree(self, commit):
        tree = git("rev-parse", commit + "^{tree}").strip().decode()
        if tree in self.trees:
            return
        entries = git("ls-tree", "-rz", "--full-tree", tree).split(b"\0")
        for entry in filter(None, entries):
            if entry in self.entries:
                continue
            self.entries.add(entry)
            self.files += 1
            if self.files > FILES:
                raise AuditError("3000-entry audit bound exceeded")
            info, path = entry.split(b"\t", 1)
            mode, kind, oid = info.split()
            self.inspect(path)
            name = PurePosixPath(path.decode("utf-8"))
            if (mode not in (b"100644", b"100755") or kind != b"blob"):
                raise AuditError("symlink or non-file entry needs separate review")
            if (any(part in FORBIDDEN or part in {".git", ".claude", "target"}
                    for part in name.parts) or name.name.startswith(".env.")
                    or path == b"DESIGN.md" or path.startswith(b"docs/design/")):
                raise AuditError("credential or unnecessary file in publication tree")
            self.blob(oid.decode())
        self.trees.add(tree)

    def ref(self, oid, remote=None):
        # Annotated tags can contain private metadata even when their tree is clean.
        for _ in range(8):
            kind = git("cat-file", "-t", oid).strip()
            if kind == b"commit":
                break
            if kind != b"tag":
                raise AuditError("only commits and commit tags can be published")
            tag = git("cat-file", "tag", oid)
            self.inspect(tag)
            oid = tag.split(b"\n", 1)[0].removeprefix(b"object ").decode()
        else:
            raise AuditError("annotated-tag depth bound exceeded")
        git("merge-base", "--is-ancestor", self.base, oid)
        exclusions = [self.base]
        if remote and remote.strip("0"):
            # The hook receives the receiver's advertised old ref. It must be
            # present locally and belong to the public line before exclusion.
            git("merge-base", "--is-ancestor", self.base, remote)
            exclusions.append(remote)
        commits = git("rev-list", "--max-count=" + str(COMMITS + 1),
                      oid, "--not", *exclusions).splitlines()
        if len(commits) > COMMITS:
            raise AuditError("300-commit audit bound exceeded")
        # Always re-audit the complete resulting tree, not merely a diff.
        self.tree(oid)
        for raw in commits:
            commit = raw.decode()
            if commit in self.commits:
                continue
            self.commits.add(commit)
            if len(self.commits) > COMMITS:
                raise AuditError("300-commit audit bound exceeded")
            # Reject an unrelated parent hidden by a merge into public history.
            git("merge-base", "--is-ancestor", self.base, commit)
            self.inspect(git("cat-file", "commit", commit))
            self.tree(commit)


def main():
    signal.signal(signal.SIGALRM, expired)
    signal.setitimer(signal.ITIMER_REAL, SECONDS)
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "--check":
            refs = [(git("rev-parse", "--verify", "--end-of-options",
                         sys.argv[2] + "^{object}").strip().decode(), None)]
        elif len(sys.argv) == 4 and sys.argv[1] == "--pre-push":
            if sys.argv[3] not in PUBLIC_URLS:
                raise AuditError("push destination is not the public Kide repository")
            data = sys.stdin.buffer.read(65537)
            if len(data) > 65536:
                raise AuditError("push-ref input bound exceeded")
            refs = []
            for line in data.splitlines():
                _, local, remote_ref, remote = line.split()
                if not remote_ref.startswith((b"refs/heads/", b"refs/tags/")):
                    raise AuditError("unexpected publication ref namespace")
                if not all(re.fullmatch(rb"[0-9a-f]{40}|[0-9a-f]{64}", x)
                           for x in (local, remote)):
                    raise AuditError("invalid push object ID")
                if local.strip(b"0"):
                    refs.append((local.decode(), remote.decode()))
        else:
            raise AuditError("usage: public_audit.py --check REF | --pre-push REMOTE URL")
        audit = Audit()
        for ref, remote in refs:
            audit.ref(ref, remote)
        print(f"Public audit passed: {len(audit.commits)} commits, "
              f"{audit.files} entries, {audit.bytes} unique blob bytes; "
              "private policy checked, zero findings.")
        return 0
    except (AuditError, OSError, ValueError, KeyError, TypeError, re.error):
        # Malformed local policy is also a failure; do not echo its private bytes.
        error = sys.exc_info()[1]
        detail = str(error) if isinstance(error, AuditError) else "invalid audit input/policy"
        print("Public audit blocked: " + detail, file=sys.stderr)
        return 1
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)


if __name__ == "__main__":
    sys.exit(main())
