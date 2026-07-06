---
description: 'Use this whenever the user asks to review a pull request by number (e.g. ''review PR #123'', ''PR 45を見て'', ''review this PR''), to evaluate a PR''s diff against generic review criteria (code-review, ai-antipattern, security, edge cases, backward compatibility, functional impact, test sufficiency/coverage) and the project''s custom review perspectives stored in ./.review/*.md, running each perspective as a parallel subagent, deduplicating overlapping findings, and producing one consolidated report; this skill only reports findings and never edits code or posts PR comments unless the user separately asks for that.'
metadata:
    github-path: skills/review-pr
    github-ref: refs/heads/main
    github-repo: https://github.com/smartcrabai/agent-skills
    github-tree-sha: 726a0002ebf2ba1f1a564660afef53a0791e2668
name: review-pr
---
# Review PR

Review a GitHub Pull Request from multiple angles at once: eight generic
engineering concerns (code-review, ai-antipattern, security, edge cases,
backward compatibility, functional impact, test sufficiency, and unit-test
coverage) plus whatever project-specific perspectives the repository has
accumulated in `./.review/`. Running each perspective as its own subagent
keeps the review focused — a single reviewer trying to hold ten different
lenses in mind at once tends to blur them together and miss things a
dedicated pass would catch. This skill is read-only: it produces a report,
nothing more, unless the user explicitly asks for follow-up actions like
posting comments.

## Phase 1 — Fetch the PR

Determine the PR number:
- Use the number given in the skill arguments or the user's message if present.
- Otherwise, run `gh pr view --json number,title` to detect the PR associated
  with the current branch.
- If that also fails (no PR for this branch, or `gh` errors), ask the user
  which PR to review rather than guessing.

Fetch metadata and diff:
- `gh pr view <number> --json title,body,author,baseRefName,headRefName,files,additions,deletions`
  — the title/body capture the PR's stated intent, which reviewers need in
  order to judge whether the diff actually does what it claims, not just
  whether the code is well-formed in isolation.
- `gh pr diff <number>` for the actual changes.
- If the PR is very large (roughly thousands of lines changed), pull the file
  list first and consider partitioning the diff across reviewers by file
  group instead of handing every subagent the entire diff — this keeps each
  subagent's context focused and avoids truncation.

## Phase 2 — Discover custom perspectives

Glob `./.review/*.md` at the repository root of the current working tree.
Each file is one custom review perspective, typically authored by the
companion skills `improve-review-from-session` and `improve-review-from-pr-gap`.
These encode lessons specific to this codebase (e.g. "this project has been
bitten by X before") that generic review criteria would never surface.

Each perspective file has YAML frontmatter (`name`, `description`, `origin`)
followed by sections such as "When to apply", "What to check", "Background",
and optionally "Examples". Read only the frontmatter and "When to apply" for
each file to judge relevance to the PR's changed files — reading the full
file for every perspective up front burns tokens on perspectives that will
turn out to be irrelevant, and a quick relevance check is usually enough to
decide. Skip perspectives that clearly do not apply (e.g. a "database
migration safety" perspective when the PR only touches CSS).

If `./.review/` does not exist or contains no `.md` files, proceed with the
generic perspectives only, and say so plainly in the final report so the user
knows no custom perspectives were available — don't silently omit them.

## Phase 3 — Parallel review

Spawn all subagents in parallel: issue every Agent/Task tool call in a single
message so they execute concurrently rather than one after another. Running
them serially would work but wastes wall-clock time for no benefit, since the
reviews are independent of each other.

Spawn:
- **Eight generic reviewers**, one per dimension below, each looking at the
  whole diff unless noted otherwise. Dimensions 1-3 mirror the `code-review`,
  `ai-antipattern`, and `security-review` skills that may already exist in
  this environment — but those skills are built to run against the LOCAL
  working tree / current branch, while this skill reviews a PR diff fetched
  with `gh pr diff <number>`. Where such a skill is available, its subagent
  should load it to obtain its authoritative criteria, then apply those
  criteria to the PR diff — it must not run the skill against the local
  tree, since the local tree and the PR diff can diverge. Regardless of
  which skill (if any) informed a reviewer, every reviewer still reports
  back in this file's own structured FINDING format below, not the skill's
  native reporting mechanism, because Phase 4's deduplication depends on a
  single consistent shape across all reviewers.
  1. **Code-review** (mirrors `code-review`) — correctness bugs (logic
     errors, wrong conditions, off-by-one mistakes, broken state handling,
     races) plus reuse/simplification/efficiency cleanups (logic that
     duplicates an existing helper, needless complexity, inefficient
     patterns). Also check whether the diff actually accomplishes what the
     PR title/body claims it does.
  2. **AI-antipattern** (mirrors `ai-antipattern`) — patterns typical of
     AI-generated code:
     - An implementation that answers a different question than what was
       actually asked; patterns that exist nowhere else in the codebase;
       overly generic solutions to specific problems.
     - Plausible-but-wrong code: hallucinated APIs (methods that don't exist
       in the library version actually in use), deprecated approaches,
       logic that is syntactically valid but semantically wrong.
     - Wiring gaps: a mechanism is implemented but never connected to its
       entry point — new parameters/fields that no caller actually passes
       (grep for `options.x ?? fallback` always taking the fallback).
     - Copy-paste repetition including repeated mistakes; the same logic
       implemented inconsistently across files; mixed integration patterns
       (e.g. a generated API client in one place, hand-written HTTP calls
       in another).
     - Redundant conditional branches calling the same function and
       differing only in optional arguments (`if (x) f(a, b, c) else
       f(a, b)`), which should collapse to one call.
     - Context misfit: naming, error handling, logging, or test style that
       deviates from project conventions without explanation.
     - Scope creep: unrequested features, premature abstraction built for a
       single call site, over-configurability, unrequested legacy/backward-
       compat shims (`LEGACY_*_MAP`, a `.transform()` normalization step
       nobody asked for).
     - Dead and unused code: old implementations left behind after a
       refactor, defensive branches contradicted by every call site,
       orphaned exports/imports, code added "for future extensibility" that
       nothing calls.
     - Fallback/default abuse that hides uncertainty instead of surfacing
       it: `user?.id ?? 'unknown'` on data that should be required,
       `catch { return ''; }`, chained `a ?? b ?? c`, default parameters
       every caller omits.
  3. **Security** (mirrors `security-review`) — injection (SQL, command,
     template), broken authn/authz, hardcoded secrets/credentials, unsafe
     deserialization, path traversal, SSRF, insecure crypto/randomness,
     sensitive data leaking into logs, insecure defaults.
  4. **Edge cases & failure paths** — missing error handling, unchecked
     failures of I/O and external calls, boundary conditions (empty, zero,
     max, unicode, concurrency), abnormal inputs, resource cleanup on error
     paths, and state left partially updated after a failure.
  5. **Backward compatibility** — changes that break existing callers:
     modified public signatures, removed or renamed functions/endpoints/
     fields, changed serialization or config formats, schema changes,
     behavioral changes existing callers rely on. Before spawning this
     reviewer, check the PR title, body, and labels for a declaration that
     breaking changes are intentional and acceptable here (e.g. a
     pre-release project, an explicit "breaking change" label, or the user
     having already said breaking is fine). If so, **skip spawning this
     reviewer entirely** and record the skip and its reason in the final
     report instead — flagging an intentional, already-acknowledged break
     as a finding is pure noise. When in doubt, spawn it anyway.
  6. **Functional impact** — loss of existing functionality (features
     removed or degraded as a side effect of the change) and new
     constraints on extensibility (hardcoded assumptions, closed-off
     extension points, designs that will block foreseeable future
     requirements).
  7. **Test sufficiency** — whether the PR adds or updates tests to match
     its behavior changes: new behavior with no new tests, changed behavior
     with stale assertions, removed behavior leaving orphaned tests behind.
  8. **Unit test coverage** — quality of coverage for the touched code,
     considering existing tests plus anything added in the PR: whether
     normal cases, error/abnormal cases, and boundary values are all
     covered, whether assertions verify actual outcomes rather than merely
     "does not throw", and whether negative cases are missing. This
     reviewer should read the relevant existing test files in the
     repository, with the same local-checkout caveat as below — the local
     tree may not match the PR's base commit.
- **One reviewer per relevant custom perspective file.** If more than 6
  perspective files are relevant, batch them into groups of up to 3 per
  subagent instead of spawning a dozen agents — this keeps the fan-out
  manageable while still giving each perspective dedicated attention.

Each subagent's prompt must include:
- The PR number and the exact command to fetch the diff itself
  (`gh pr diff <number>`) — subagents start with no context, so they need to
  pull the diff themselves rather than relying on a paraphrase.
- The PR title and body, for the same "does the diff match the stated
  intent" reasoning as above.
- For dimensions 1-3, a note that the corresponding skill (if present) is
  for criteria only and must be applied to the fetched PR diff, never run
  directly against the local working tree.
- For custom-perspective reviewers, the full text of the perspective
  file(s) they are applying.
- The required structured output format below, so that deduplication in
  Phase 4 can be done mechanically rather than by re-reading prose:

```
FINDING
file: <path>
line: <number or range in the new version>
severity: critical | major | minor
perspective: <generic dimension name or custom perspective name>
summary: <one sentence>
detail: <why it is a problem and a concrete failure scenario>
```

Instruct every subagent to:
- Report only findings it is confident about and can back with a concrete
  failure scenario — not stylistic nitpicks or speculative "might be an
  issue" hedges. A review report loses credibility fast if it's padded with
  low-conviction noise.
- Return exactly `NO FINDINGS` if nothing meets that bar.
- Feel free to read surrounding code in the repository for context, but keep
  in mind the local checkout may not match the PR's diff exactly (e.g. the
  PR may be based on an older commit, or the working tree may have moved on)
  — treat the diff as the source of truth for what changed.

## Phase 4 — Deduplicate and consolidate

Two findings are duplicates when they point at the same root cause at the
same location (same file, overlapping line ranges) — even if the wording
differs or they came from different perspectives. When merging duplicates:
- Keep the highest severity reported and the clearest explanation.
- List every perspective that flagged it. Convergence across independent
  perspectives is itself a signal worth surfacing — if both the security
  reviewer and a custom perspective independently flagged the same line,
  that's stronger evidence than either alone, and the report should say so.

Before finalizing, spot-check questionable or surprising findings against
the actual diff. Subagents occasionally misread context or flag something
that a closer look shows is already handled elsewhere in the diff; discard
findings that don't hold up.

## Phase 5 — Report

Write the report in the language the user is conversing in — this skill
file is in English, but the output should match the user, not the skill.

Use this template:

```
# Review Report: PR #<number> — <title>

## Summary
<1-3 sentences: what the PR does, overall assessment, counts by severity>

## Findings

### Critical
- `<file>:<line>` — <summary> (perspectives: <list>)
  <detail>

### Major
- `<file>:<line>` — <summary> (perspectives: <list>)
  <detail>

### Minor
- `<file>:<line>` — <summary> (perspectives: <list>)
  <detail>

## Perspectives applied
- Generic: code-review, ai-antipattern, security, edge-cases & failure paths, backward-compatibility (note if skipped and why), functional impact, test sufficiency, unit-test coverage
- Custom (./.review/): <list of files used, and any skipped as irrelevant, with a one-line reason each>
```

If a severity bucket has no findings, keep the heading and write "None."
rather than omitting it — that confirms the dimension was actually checked.

State explicitly, at the end of the report, that this was a report-only
review: no commits were pushed and no comments were posted to the PR, and
that either of those would only happen if the user asks separately.
