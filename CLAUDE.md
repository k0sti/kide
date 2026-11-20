@AGENTS.md

All active work is in public `k0sti/kide`, on `dev` or a task branch. The private
historical repository is archived and must not be a push target. Squash-merges
to `main` need Kosti's explicit OK and a fresh audit of the squash commit.

Install the pre-push hook and private local audit policy per CONTRIBUTING.md:
every push, including development work, must pass the bounded public-data audit.
Never bypass a failed or incomplete audit. Keep design documents in Kota at
`shared/kota-development/Kide/` and unnecessary files out of the repository.
AGENTS.md supersedes earlier automatic merge/push-to-main instructions.
