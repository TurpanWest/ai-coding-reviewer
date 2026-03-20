#!/bin/sh
set -e

# ── Generate diff ─────────────────────────────────────────────────────────────
# In a pull_request context GITHUB_BASE_REF is the target branch (e.g. "main")
if [ -z "$GITHUB_BASE_REF" ]; then
  echo "::error::GITHUB_BASE_REF is not set. This action must run on a pull_request event."
  exit 2
fi

git diff "origin/${GITHUB_BASE_REF}...HEAD" > /tmp/pr.diff

DIFF_LINES=$(wc -l < /tmp/pr.diff)
echo "Diff size: ${DIFF_LINES} lines"

# ── Build CLI args ────────────────────────────────────────────────────────────
ARGS="--diff /tmp/pr.diff"
ARGS="$ARGS --policy ${GITHUB_WORKSPACE}/${INPUT_POLICY}"
ARGS="$ARGS --source-root ${GITHUB_WORKSPACE}"
ARGS="$ARGS --output ${GITHUB_WORKSPACE}/${INPUT_OUTPUT}"

[ -n "$INPUT_THRESHOLD" ]        && ARGS="$ARGS --threshold $INPUT_THRESHOLD"
[ -n "$INPUT_REVIEWER_1" ]       && ARGS="$ARGS --reviewer-1 $INPUT_REVIEWER_1"
[ -n "$INPUT_REVIEWER_1_MODEL" ] && ARGS="$ARGS --reviewer-1-model $INPUT_REVIEWER_1_MODEL"
[ -n "$INPUT_REVIEWER_2" ]       && ARGS="$ARGS --reviewer-2 $INPUT_REVIEWER_2"
[ -n "$INPUT_REVIEWER_2_MODEL" ] && ARGS="$ARGS --reviewer-2-model $INPUT_REVIEWER_2_MODEL"
[ -n "$INPUT_MAX_DIFF_LINES" ]   && ARGS="$ARGS --max-diff-lines $INPUT_MAX_DIFF_LINES"

# ── Run review ────────────────────────────────────────────────────────────────
# shellcheck disable=SC2086
ai-reviewer $ARGS
EXIT_CODE=$?

# ── Set outputs ───────────────────────────────────────────────────────────────
if [ $EXIT_CODE -eq 0 ]; then
  echo "verdict=pass" >> "$GITHUB_OUTPUT"
else
  echo "verdict=fail" >> "$GITHUB_OUTPUT"
fi
echo "report-path=${GITHUB_WORKSPACE}/${INPUT_OUTPUT}" >> "$GITHUB_OUTPUT"

exit $EXIT_CODE
