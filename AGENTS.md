# AGENTS.md

This file provides guidance to Codex and other coding agents when working in
this repository.

## PR Review Gate

Before creating a pull request, run a code-review pass against the branch diff:
findings first, ordered by severity, with file/line references; then open
questions, then a brief summary and validation gaps. Address or explicitly
accept any findings before opening the PR.

After the review pass is complete, run `scripts/pre_pr_review.sh mark` to record
the reviewed HEAD. Run `scripts/pre_pr_review.sh check` before creating the PR.
Use `git config core.hooksPath .githooks` in local clones to install the
versioned `.githooks/pre-push` hook. The hook blocks pushes from `codex/*`
branches when the review marker is missing or stale.

## Commit Conventions

Do not add `Co-Authored-By:` trailers for AI agents when creating commits. The
human author of record takes full ownership of the change, and AI co-author
trailers can interfere with CLA checks. Write normal commit messages without AI
attribution trailers.
