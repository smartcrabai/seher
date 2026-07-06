---
description: Use this whenever the user asks to review uncommitted/local/working-tree changes, check the current diff before committing, or evaluate pending changes against the project's custom review perspectives in ./.review/; it gathers staged, unstaged, and untracked changes, runs generic and project-specific reviewers in parallel subagents, deduplicates their findings, and returns one consolidated report without modifying any code.
metadata:
    github-path: skills/review-uncommitted
    github-ref: refs/heads/main
    github-repo: https://github.com/smartcrabai/agent-skills
    github-tree-sha: d82b57e9bd6ae02c7e4d31932b783517cf64ec8b
name: review-uncommitted
---
# Review Uncommitted Changes

Review everything in the working tree that has not yet been committed — staged
changes, unstaged changes, and new untracked files — against both generic
software-quality lenses and any project-specific review perspectives the
repository has accumulated in `./.review/`. Run the reviews concurrently so
the wall-clock cost stays low even as the number of perspectives grows, then
merge overlapping findings before showing anything to the user.

This skill is read-only. It never edits files, stages changes, or suggests
git commands that mutate the tree. Its only output is a report. Fixing issues
is a separate step the user decides to take afterward — conflating review
and fixing tends to produce hasty patches and hides the tradeoffs the user
should weigh.

## Phase 1 — Gather the diff

Collect the full picture of what changed before reviewing anything, otherwise
subagents will each re-derive a partial (and possibly inconsistent) view of
the diff:

1. `git diff` — unstaged changes to tracked files.
2. `git diff --cached` — staged changes.
3. `git status --porcelain` — to find untracked files (`??` entries).
4. For untracked files that look like source (not binaries, lockfiles, or
   generated output), read their contents so they can be reviewed too — a
   brand-new file with a bug is just as worth catching as a modified line.
   Skip untracked files that are large, binary, or clearly vendored/generated;
   reviewing those wastes tokens without surfacing real issues.

If all three commands come back empty, tell the user there is nothing to
review and stop here. Do not invent findings against an empty diff.

## Phase 2 — Discover custom perspectives

Custom perspectives are project-specific review lenses that generic reviewers
would not know to apply — things like "this codebase forbids raw SQL string
concatenation" or "all new API routes must have a corresponding rate limit."
They typically accumulate over time via the companion skills
`improve-review-from-session` and `improve-review-from-pr-gap`, and live as one file
per perspective under `./.review/*.md` in the repository root.

1. Glob `./.review/*.md`. If the directory does not exist or matches no
   files, proceed with generic perspectives only, and say so plainly in the
   final report — the user should know no project-specific lenses were
   applied, not just see a clean report and assume full coverage.
2. Each perspective file has YAML frontmatter (`name`, `description`,
   `origin`) followed by sections such as "When to apply", "What to check",
   "Background", and optionally "Examples". At this stage, read only the
   frontmatter and "When to apply" section for each file — not the whole
   file. That is enough to judge whether the perspective is relevant to the
   files actually touched in this diff. Reading every perspective in full
   before knowing it's relevant burns tokens on lenses that will never fire
   and, worse, invites reviewers to force-fit irrelevant checks onto the
   diff, producing noise.
3. Drop perspectives that are clearly irrelevant (e.g., a "database migration
   safety" perspective when the diff only touches CSS). Keep the rest for
   Phase 3, where their full content gets loaded by the subagent assigned to
   them.

## Phase 3 — Parallel review

Spawn all reviewer subagents in one message with multiple Agent tool calls so
they execute concurrently — sequential review would multiply latency by the
number of perspectives for no benefit, since each reviewer only needs the
diff and one lens, not the others' output.

Reviewers to spawn:

- **Eight generic reviewers**, one per dimension, each looking at the *same*
  full diff from a different angle. Dimensions 1-3 mirror companion skills
  that may be installed in this environment (`code-review`, `ai-antipattern`,
  `security-review`); instruct that reviewer to load the same-named skill if
  it is available, for whatever authoritative or more current criteria it
  adds on top of the summary below. Regardless of whether the skill exists,
  the reviewer must still return its results in this file's FINDING format
  (below), not the skill's own reporting mechanism — Phase 4's
  deduplication depends on one consistent structure across every reviewer,
  generic or custom.
  1. **Code-review** — correctness bugs (logic errors, wrong conditions,
     off-by-one errors, broken state handling, race conditions) plus
     reuse/simplification/efficiency cleanups (logic duplicating an existing
     helper, needless complexity, inefficient patterns). Load the
     `code-review` skill if available.
  2. **AI-antipattern** — patterns typical of AI-generated code. Load the
     `ai-antipattern` skill if available; otherwise check for:
     - An implementation that answers a different question than what was
       asked; patterns that exist nowhere else in the codebase; overly
       generic solutions to a narrow problem.
     - Plausible-but-wrong code: hallucinated APIs (methods that don't exist
       in the library version actually in use), deprecated approaches, logic
       that is syntactically valid but semantically wrong.
     - Wiring gaps: a mechanism is implemented but never connected to its
       entry point — new parameters or fields that no caller actually passes
       (grep for patterns like `options.x ?? fallback` and confirm the value
       is ever anything but the fallback).
     - Copy-paste repetition, including repeated mistakes; the same logic
       implemented inconsistently across files; mixed integration patterns
       (a generated API client in one place, a hand-written HTTP call in
       another).
     - Redundant conditional branches that call the same function and differ
       only in optional arguments (`if (x) f(a, b, c) else f(a, b)`), which
       should collapse to one call.
     - Context misfit: naming, error handling, logging, or test style that
       deviates from project conventions without explanation.
     - Scope creep: unrequested features, premature abstraction for a single
       call site, over-configurability, unrequested legacy/backward-compat
       shims (`LEGACY_*_MAP`, an unasked-for `.transform()` normalization).
     - Dead and unused code: old implementations left behind after a
       refactor, defensive branches contradicted by every call site, orphaned
       exports/imports, "future extensibility" code nothing calls.
     - Fallback/default abuse that hides uncertainty instead of surfacing it:
       `user?.id ?? 'unknown'` on data that should be required, a `catch`
       that swallows the error and returns an empty value, chained
       `a ?? b ?? c`, default parameters every caller omits.
  3. **Security** — injection (SQL, command, template), broken authn/authz,
     hardcoded secrets or credentials, unsafe deserialization, path
     traversal, SSRF, insecure crypto or randomness, sensitive data leaking
     into logs, insecure defaults. Load the `security-review` skill if
     available.
  4. **Edge cases & failure paths** — missing error handling, unchecked
     failures of I/O and external calls, boundary conditions (empty, zero,
     max, unicode, concurrency), abnormal inputs, resource cleanup on error
     paths, partially-failed state left behind.
  5. **Backward compatibility** — changes that break existing callers:
     modified public signatures, removed or renamed functions, endpoints, or
     fields, changed serialization or config formats, schema changes,
     behavioral changes existing callers rely on. Before spawning this
     reviewer, judge from what the user has said and any available project
     context whether breaking changes are explicitly acceptable here (a
     pre-release project, the user said breaking is fine, a declared
     intentional break). If so, skip spawning this reviewer entirely —
     flagging an intentional break as a finding is pure noise — and record
     the skip and its reason in the final report instead. When in doubt, run
     the reviewer; a false skip hides real regressions, while a false run
     only costs one subagent's worth of tokens.
  6. **Functional impact** — loss of existing functionality (features
     removed or degraded as a side effect of the change) and new constraints
     on extensibility (hardcoded assumptions, closed-off extension points,
     designs that will block foreseeable future requirements).
  7. **Test sufficiency** — whether the diff adds or updates tests to match
     its behavior changes: new behavior with no new tests, changed behavior
     with stale assertions, removed behavior leaving orphaned tests behind.
  8. **Unit test coverage** — quality of coverage for the touched code,
     considering existing tests plus any added in the diff: normal cases,
     error/abnormal cases, and boundary values all covered; assertions that
     verify actual outcomes rather than merely "does not throw"; missing
     negative cases.

     Reviewers 7 and 8 cannot judge sufficiency or quality from the diff
     alone — they need to read the relevant existing test files too, not
     just what changed, to know what was already covered before this diff
     and what remains uncovered after it.
- **One reviewer per relevant custom perspective file** from Phase 2. If more
  than 6 perspective files are relevant, group them into batches of up to 3
  per subagent instead of spawning one agent per file — this keeps the agent
  count manageable without losing coverage, since a single subagent can
  competently apply a few related lenses to the same diff in one pass.

Each subagent's prompt must include, self-contained (subagents share no
context with each other or with prior conversation):

- The full diff content itself, or the exact `git diff` / `git diff --cached`
  commands plus untracked file paths needed to reproduce it — do not assume
  the subagent can infer what changed.
- For custom-perspective reviewers, the complete perspective file content
  (frontmatter + all sections), not just the "When to apply" summary used
  for filtering.
- For the test-sufficiency and unit-test-coverage reviewers (dimensions 7 and
  8), the paths of the existing test files relevant to the touched code, and
  an explicit instruction to read them — grading coverage from the diff
  alone misses what already existed.
- The required output format below, stated explicitly so that deduplication
  in Phase 4 is mechanical rather than a re-interpretation exercise:

```
FINDING
file: <path>
line: <number or range>
severity: critical | major | minor
perspective: <generic dimension name or custom perspective name>
summary: <one sentence>
detail: <why it is a problem and a concrete failure scenario>
```

Instruct every subagent to:

- Report only findings it is confident about and that come with a concrete
  failure scenario (an input, sequence, or condition that triggers the
  problem) — not stylistic nitpicks or speculative "could be an issue"
  hedges. A review flooded with low-confidence noise is worse than a shorter
  one the user can trust.
- Return the literal text `NO FINDINGS` if nothing meets that bar, instead of
  manufacturing a finding to seem useful.

## Phase 4 — Deduplicate and consolidate

Collect every subagent's output. Two findings are duplicates when they point
at the same root cause at the same location (same file, overlapping line
ranges) — even if phrased differently or surfaced from different
perspectives. This happens often and legitimately: a SQL injection flaw is
naturally caught by both the security reviewer and a custom "data access"
perspective.

When merging duplicates:

- Keep the highest severity among the duplicates.
- Keep whichever explanation is clearest and most specific about the failure
  scenario.
- List every perspective that flagged it. Multiple independent perspectives
  converging on the same location is a strong signal the issue is real and
  worth prioritizing — call this out in the report rather than silently
  collapsing it to one line.

Before finalizing, spot-check findings that look questionable (surprising
severity, unclear reasoning, or contradicting what the diff actually shows)
against the real file content. Discard any finding that does not hold up —
a false positive undermines trust in the whole report more than a missed
minor issue does.

## Phase 5 — Report

Write the report in the language the user has been conversing in — this
SKILL.md is in English, but the output is for the user, not for a document
in this repository. Use this structure:

```
# Review Report: Uncommitted Changes
## Summary
<1-3 sentences: scope of the diff, overall assessment, counts by severity>
## Findings
### Critical
- `<file>:<line>` — <summary> (perspectives: <list>)
  <detail>
### Major
...
### Minor
...
## Perspectives applied
- Generic: code-review, ai-antipattern, security, edge cases & failure
  paths, backward compatibility (or: skipped — <reason>), functional
  impact, test sufficiency, unit-test coverage
- Custom (./.review/): <list of files used, and any skipped as irrelevant>
```

If there are no findings in a severity bucket, omit that subsection rather
than showing it empty. If there are no findings at all, say so clearly and
still list which perspectives were applied, so the user knows the absence of
findings reflects a real check rather than a skipped one.

End the report by stating plainly that this skill only reports issues and
does not modify any files — any fix is a separate, deliberate step the user
takes next.
