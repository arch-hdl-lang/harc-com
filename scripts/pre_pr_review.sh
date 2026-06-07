#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/pre_pr_review.sh mark [base-ref] --reviewer-kind separate-agent --reviewer <id>
  scripts/pre_pr_review.sh mark [base-ref] --reviewer-kind human --reviewer <name>
  scripts/pre_pr_review.sh check [base-ref]

Records or checks that the current branch HEAD has received a code-review pass
before it is pushed/opened as a PR. The review must be performed by an
independent reviewer: either a separate agent/thread or a human reviewer.
Review markers are local and stored under .git/pre-pr-reviews/.

Environment alternatives:
  PRE_PR_REVIEWER_KIND=separate-agent PRE_PR_REVIEWER=<id> scripts/pre_pr_review.sh mark
  PRE_PR_REVIEWER_KIND=human PRE_PR_REVIEWER=<name> scripts/pre_pr_review.sh mark
USAGE
}

mode="${1:-}"
if [[ $# -gt 0 ]]; then
  shift
fi

if [[ "$mode" != "mark" && "$mode" != "check" ]]; then
  usage >&2
  exit 2
fi

base_ref="${PRE_PR_REVIEW_BASE:-origin/main}"
reviewer="${PRE_PR_REVIEWER:-}"
reviewer_kind="${PRE_PR_REVIEWER_KIND:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base|--base-ref)
      if [[ $# -lt 2 ]]; then
        echo "pre-pr-review: $1 requires a value" >&2
        exit 2
      fi
      base_ref="$2"
      shift 2
      ;;
    --reviewer)
      if [[ $# -lt 2 ]]; then
        echo "pre-pr-review: --reviewer requires a value" >&2
        exit 2
      fi
      reviewer="$2"
      shift 2
      ;;
    --reviewer-kind)
      if [[ $# -lt 2 ]]; then
        echo "pre-pr-review: --reviewer-kind requires a value" >&2
        exit 2
      fi
      reviewer_kind="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    -*)
      echo "pre-pr-review: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      base_ref="$1"
      shift
      ;;
  esac
done

validate_reviewer_kind() {
  case "$1" in
    separate-agent|human) return 0 ;;
    *) return 1 ;;
  esac
}

marker_field() {
  local field="$1"
  local file="$2"
  awk -v key="$field" '
    index($0, key "=") == 1 {
      sub("^[^=]*=", "")
      print
      exit
    }
  ' "$file"
}

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

branch="$(git branch --show-current)"
if [[ -z "$branch" ]]; then
  echo "pre-pr-review: detached HEAD is not supported" >&2
  exit 2
fi

head_sha="$(git rev-parse HEAD)"
safe_branch="$(printf '%s' "$branch" | tr '/: ' '___')"
marker_dir="$(git rev-parse --git-path pre-pr-reviews)"
marker="${marker_dir}/${safe_branch}.review"

mkdir -p "$marker_dir"

changed_files="$(git diff --name-only "${base_ref}...HEAD" 2>/dev/null || true)"
if [[ -z "$changed_files" ]]; then
  changed_files="$(git diff --name-only HEAD~1...HEAD 2>/dev/null || true)"
fi

if [[ "$mode" == "mark" ]]; then
  if [[ -z "$reviewer_kind" || -z "$reviewer" ]]; then
    cat >&2 <<EOF
pre-pr-review: mark requires independent reviewer provenance.

Run an independent review in a separate agent/thread or by a human reviewer,
then record it with one of:
  scripts/pre_pr_review.sh mark --reviewer-kind separate-agent --reviewer <agent-or-thread-id>
  scripts/pre_pr_review.sh mark --reviewer-kind human --reviewer <reviewer-name>
EOF
    exit 2
  fi

  if ! validate_reviewer_kind "$reviewer_kind"; then
    echo "pre-pr-review: --reviewer-kind must be 'separate-agent' or 'human'" >&2
    exit 2
  fi

  case "$reviewer" in
    *$'\n'*|*$'\r'*)
      echo "pre-pr-review: --reviewer must be a single-line value" >&2
      exit 2
      ;;
  esac

  {
    printf 'branch=%s\n' "$branch"
    printf 'head=%s\n' "$head_sha"
    printf 'base=%s\n' "$base_ref"
    printf 'reviewer_kind=%s\n' "$reviewer_kind"
    printf 'reviewer=%s\n' "$reviewer"
    printf 'reviewed_at_utc=%s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    printf 'changed_files<<EOF\n%s\nEOF\n' "$changed_files"
  } >"$marker"
  echo "pre-pr-review: recorded ${reviewer_kind} review marker for ${branch} at ${head_sha}"
  exit 0
fi

if [[ ! -f "$marker" ]]; then
  cat >&2 <<EOF
pre-pr-review: missing code-review marker for ${branch}.

Before creating/pushing a PR, run a code-review pass against:
  git diff ${base_ref}...HEAD

Review stance:
  findings first, ordered by severity, with file/line references;
  then open questions, then a brief summary and validation gaps.

After the review is complete and findings are addressed or accepted, run:
  scripts/pre_pr_review.sh mark --reviewer-kind separate-agent --reviewer <agent-or-thread-id>

Or, for human review:
  scripts/pre_pr_review.sh mark --reviewer-kind human --reviewer <reviewer-name>
EOF
  exit 1
fi

reviewed_head="$(marker_field head "$marker")"
if [[ "$reviewed_head" != "$head_sha" ]]; then
  cat >&2 <<EOF
pre-pr-review: marker is stale for ${branch}.
  reviewed: ${reviewed_head:-<none>}
  current : ${head_sha}

Run a fresh code-review pass, then:
  scripts/pre_pr_review.sh mark --reviewer-kind separate-agent --reviewer <agent-or-thread-id>
EOF
  exit 1
fi

marker_reviewer_kind="$(marker_field reviewer_kind "$marker")"
marker_reviewer="$(marker_field reviewer "$marker")"
if [[ -z "$marker_reviewer_kind" || -z "$marker_reviewer" ]]; then
  cat >&2 <<EOF
pre-pr-review: marker for ${branch} lacks independent reviewer provenance.

Old same-session markers are no longer accepted. Run an independent review,
then record it with:
  scripts/pre_pr_review.sh mark --reviewer-kind separate-agent --reviewer <agent-or-thread-id>
EOF
  exit 1
fi

if ! validate_reviewer_kind "$marker_reviewer_kind"; then
  echo "pre-pr-review: marker has invalid reviewer_kind: ${marker_reviewer_kind}" >&2
  exit 1
fi

echo "pre-pr-review: review marker is current for ${branch} (${marker_reviewer_kind}: ${marker_reviewer})"
