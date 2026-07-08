# CI skipped-check probe (task #45)

Scratch marker for a one-shot experiment: the PR carrying this file
guards the required `test (…)` jobs with
`if: github.head_ref != 'ci-skip-probe'`, so its own pull_request runs
mint **created-then-skipped** required check runs while its merge-group
gate still executes normally on the fleet.

Question under test: does the merge queue treat a skipped required
check as satisfied for queue entry (GitHub's documented behavior), or
does it block like the never-created / "Expected" wedge?

- If this PR merges: answer is yes — the follow-up PR removes this file
  and the guard, and the two-job hosted-relevance pattern (doc-only PRs
  off the fleet) becomes viable.
- If it wedges: answer is no — close the PR, delete the branch, keep
  the in-job fast-path relevance checks.
