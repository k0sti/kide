Independent Python reference for the Kide format (§2, §3.1, §5.1–5.3 of `crates/kide/SPEC.md`), standard library only.

- `python3 -I kide_ref.py` prints the §8 test vectors as Markdown table rows.
- `python3 -I kide_ref.py --check` runs the self-checks and exits non-zero on failure.
