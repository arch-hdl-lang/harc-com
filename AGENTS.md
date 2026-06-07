# AGENTS.md

This file provides guidance to Codex and other coding agents when working in
this repository.

## PR Review Gate

Before creating a pull request, request an independent code-review pass against
the branch diff from a separate agent/thread or a human reviewer. The review
must use a review stance: findings first, ordered by severity, with file/line
references; then open questions, then a brief summary and validation gaps.
Address or explicitly accept any findings before opening the PR.

After the independent review pass is complete, record the reviewed HEAD with
reviewer provenance:

```bash
scripts/pre_pr_review.sh mark --reviewer-kind separate-agent --reviewer <agent-or-thread-id>
```

Use `--reviewer-kind human --reviewer <reviewer-name>` for human reviews. Run
`scripts/pre_pr_review.sh check` before creating the PR. Use
`git config core.hooksPath .githooks` in local clones to install the versioned
`.githooks/pre-push` hook. The hook blocks pushes from `codex/*` branches when
the review marker is missing, stale, or lacks independent reviewer provenance.

## Commit Conventions

Do not add `Co-Authored-By:` trailers for AI agents when creating commits. The
human author of record takes full ownership of the change, and AI co-author
trailers can interfere with CLA checks. Write normal commit messages without AI
attribution trailers.
