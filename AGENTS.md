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
reviewer provenance attestation:

```bash
scripts/pre_pr_review.sh mark --reviewer-kind separate-agent --reviewer <agent-or-thread-id>
```

Use `--reviewer-kind human --reviewer <reviewer-name>` for human reviews. The
hook validates the attestation fields and reviewed SHA; it cannot independently
prove reviewer identity. Run `scripts/pre_pr_review.sh check` before creating
the PR. Use `git config core.hooksPath .githooks` in local clones to install the
versioned `.githooks/pre-push` hook. The hook blocks pushes from `codex/*`
branches when the review marker is missing, stale, or lacks reviewer provenance
attestation.

## Commit Conventions

Do not add `Co-Authored-By:` trailers for AI agents when creating commits. The
human author of record takes full ownership of the change, and AI co-author
trailers can interfere with CLA checks. Write normal commit messages without AI
attribution trailers.

## Worktrees Share One Stash — Never Use Plain `git stash`

This repo is routinely checked out into ~10 simultaneous worktrees, one per
agent session. A worktree isolates the **checkout** — working files, `HEAD`,
index, `ORIG_HEAD`, reflogs. It does **not** isolate the repository. The object
database, `refs/heads`, `refs/remotes` and `refs/stash` all live in the common
`.git` directory:

```
.git/refs/stash                                 <- shared: one stack per repo
.git/worktrees/<name>/refs/                     <- empty; no per-worktree stash exists
```

So there is exactly one stash stack for all worktrees, and `git stash pop` from
any of them pops whatever is on top regardless of which session pushed it. Git
provides no per-worktree stash.

Two properties combine into a data-loss trap:

1. `git stash` on an already-clean tree saves **nothing** — and reports it only
   on a line that `-q` suppresses.
2. `git stash pop` always pops the shared top of stack.

An agent that stashes (no-op), switches branches to check something, and pops
to "restore" therefore pops a *different session's* entry, usually landing in
conflict across files it never touched. This has happened.

### Use a per-worktree ref

`refs/worktree/*` is one of git's genuinely per-worktree ref namespaces, and
`git stash create` writes a stash commit **without** touching `refs/stash`:

```bash
# park
SHA=$(git stash create "wip: <what>")
git update-ref refs/worktree/wip "$SHA"

# ... check out another ref, run a baseline, whatever ...

# restore
git stash apply refs/worktree/wip
git update-ref -d refs/worktree/wip
```

Every worktree can use the literal name `refs/worktree/wip` with no collision —
the ref is written under `.git/worktrees/<name>/refs/worktree/` and is
unresolvable from any other worktree.

The no-op case now fails **loudly** instead of silently: on a clean tree
`git stash create` prints nothing, so `update-ref` gets an empty argument and
dies with `fatal: : not a valid SHA1`. The failure happens at save time, before
any restore can go wrong.

### Caveats

- `git stash create` does **not** capture untracked files; there is no `-u`
  equivalent. `git add` the file first if you need it parked.
- `git add -N` (intent-to-add) does not work around that — it makes
  `git stash create` fail outright with
  `error: Entry '<file>' not uptodate. Cannot merge.`

### Prefer not parking at all

For the common case — "is this test failure pre-existing on `main`?" — do not
dirty the tree in the first place. Commit your work, or `git checkout <ref>`
and back from an already-clean tree. Neither touches shared state.

### Branches are shared too

The same common `.git` holds `refs/heads`, so a force-push or `git branch -D`
from one worktree is visible to every other worktree on this repo. Verify with
`git worktree list` before deleting or force-updating any branch you did not
create in this session.
