---
description: 'Use this when the user says a merged PR turned out to have an oversight/missed case that needed a follow-up fix PR, and wants to extract the review perspectives that would have caught it — e.g. ''PR #123 had a miss, analyze the gap'', ''対応漏れがあったPR 123から観点を抽出して''; it compares the original PR and its fix PR, determines which gaps were actually catchable at review time, and writes or strengthens perspective files under ./.review/ for the companion skills review-uncommitted and review-pr to apply going forward.'
metadata:
    github-path: skills/improve-review-from-pr-gap
    github-ref: refs/heads/main
    github-repo: https://github.com/smartcrabai/agent-skills
    github-tree-sha: 28ebc98804b94a42a6c6ffe441eafec04b1ed357
name: improve-review-from-pr-gap
---
# Improve Review from PR Gap

When a bug slips through review and needs a follow-up fix, the fix PR is
direct evidence of a blind spot in how the original PR was reviewed. Treating
that as a one-off incident wastes the lesson; treating it as a signal to
extract a reusable review perspective means the next PR with the same shape
of problem gets caught before merge. This skill turns one incident into a
durable check under `./.review/`, consumed later by the companion skills
`review-pr` and `review-uncommitted`.

The output is not a postmortem — it is one or more small perspective files
that a future reviewer (human or subagent) can apply mechanically to an
unrelated diff. Only write a perspective for something that was actually
detectable by reading the original diff; anything that required production
behavior or runtime data to notice is not something a review perspective can
fix, and pretending otherwise just adds noise to `./.review/`.

## Step 1 — Identify the PR pair

Determine the original PR number from the skill arguments or the user's
message — this is the PR that had the oversight. If the user also supplies
the fix PR number, use it directly and skip the search below; don't
second-guess an explicit input.

If the fix PR is not given, locate it:

- `gh pr list --state all --search "<original-number>"` — catches fix PRs
  that reference the original by number in their title or body.
- `gh pr view <original-number> --comments` — look for cross-references such
  as "fixes the miss in #<n>", "follow-up to #<n>", or reviewers/authors
  discussing a gap after merge.
- `gh pr list --state all --json number,title,author,mergedAt,files` —
  filter for PRs by the same author, touching the same files, merged shortly
  after the original. A fix for a missed case is usually small, close in
  time, and overlaps the original's file set.
- `gh pr list --state all --search "fix"` filtered by recency, as a fallback
  when the above turn up nothing.

If multiple candidates remain plausible, pick the most likely one (closest in
time, largest file overlap, most direct textual reference), state that
assumption explicitly in the final report, and list the other candidates so
the user can correct course cheaply. If no candidate is found at all, say so
and ask the user for the fix PR number — guessing at a fix PR and analyzing
the wrong diff produces perspectives that don't match the actual gap, which
is worse than asking.

## Step 2 — Gather evidence

For both the original and fix PR:

- `gh pr view <n> --json title,body,author,mergedAt,baseRefName,files`
- `gh pr diff <n>`

Also fetch what reviewers actually said on the original PR:

- `gh pr view <original-number> --comments`
- `gh api repos/{owner}/{repo}/pulls/<original-number>/comments`

This step matters because it separates two very different failure modes: a
reviewer who never noticed the issue (a genuine perspective gap) versus a
reviewer who raised it and was overruled or ignored (a process failure, not a
missing perspective — no new `./.review/` file fixes that). Only the former
is actionable by this skill.

Finally, read the current `./.review/*.md` files — frontmatter and "When to
apply" section only, not the full body — to know which perspectives already
exist before deciding whether a gap is new or a strengthening of something
already there.

## Step 3 — Analyze the gap

This step can be delegated to a subagent. Give it, verbatim:

- Both PR diffs (`gh pr diff` output).
- Both PR bodies/titles.
- The original PR's review comments.
- The name + description (not full body) of every existing `./.review/*.md`
  file.

Instruct the subagent to answer, for each distinct logical change in the fix
PR (a fix PR touching three unrelated call sites is three separate
questions, not one):

1. **What defect or omission in the original PR does this change address?**
   State it concretely — the specific condition, input, or code path the
   original PR failed to handle.
2. **Was this detectable from the original diff alone, at review time?** A
   missing null check, an unhandled enum case, a copy-pasted condition with
   the wrong variable — these are visible in a diff. A race condition that
   only manifests under production load, or a third-party API behavior
   change, is not. Only the former can become a review perspective — a
   perspective describing something no reviewer could have applied by
   reading the diff is not a check, it's a wish. Discard the latter from
   consideration for Step 4, but keep it for the report's "not converted"
   section.
3. **What generalized, reusable check would have caught it?** Phrase it as a
   perspective that applies to future, unrelated diffs sharing the same
   *shape* of problem — not as a description of this specific incident. E.g.
   generalize "PR #123 forgot to handle the `cancelled` status in the
   invoice state machine" into "when a diff adds or branches on an enum/union
   value, verify every switch/match over that type is updated" — not "check
   that invoice cancellation is handled."
4. **Does an existing `./.review/` perspective already cover this?** If yes,
   this is an APPLICATION failure (the perspective existed but wasn't applied
   or wasn't specific enough to catch this case), not a missing perspective.
   In that case the right fix is to sharpen the existing file — add a
   concrete "What to check" item and/or an example drawn from this incident —
   rather than create a near-duplicate file. Duplicated perspectives dilute
   reviewer attention in `review-pr`/`review-uncommitted` without adding
   coverage.

## Step 4 — Write perspective files

Create `./.review/` at the repository root if it does not exist yet. For
each gap confirmed in Step 3 as (a) detectable at review time and (b) not
already covered:

- One file per new perspective, kebab-case filename matching the `name`
  field (e.g. `enum-exhaustiveness-check.md`).
- For gaps that overlap an existing perspective, edit that file in place —
  do not create a second file for the same concern.

Use this exact format — the companion skills `review-pr` and
`review-uncommitted` parse the frontmatter and headings mechanically, so
keep the structure stable:

```markdown
---
name: <kebab-case-id>
description: <one line — what this perspective checks>
origin: gap analysis of PR #<original> vs PR #<fix>
---

# <Title>

## When to apply
<which file types, code patterns, or change types this applies to — used by review skills to decide relevance>

## What to check
- <concrete, checkable items — imperative>

## Background
<what the original PR missed and how the fix PR revealed it, 2-4 sentences>

## Examples
### Bad
<minimal snippet distilled from the original PR's miss>
### Good
<corrected version distilled from the fix PR>
```

Keep "When to apply" specific enough that `review-pr`/`review-uncommitted`
can skip this perspective on unrelated diffs (e.g. "diffs that add a case to
an enum/union type or a switch/match over one") — a vague "When to apply"
like "backend code" forces every future review to load the full file just to
find out it doesn't apply, which is the exact inefficiency those skills are
designed to avoid. Keep "What to check" as imperative, mechanically
checkable items, not restatements of the background story — a reviewer
subagent applies these directly to a diff it has never seen before.

## Step 5 — Report

Use this template, filled in with the actual findings:

```
# Review Gap Analysis: PR #<original> → PR #<fix>
## What the fix PR addressed
<bullet list of defects/omissions>
## Perspectives written
- `./.review/<file>.md` (new|updated) — <one-line summary>
## Not converted into perspectives
<gaps only detectable at runtime, with reasoning>
```

If Step 1 required guessing the fix PR among multiple candidates, state that
assumption at the top of the report along with the other candidates
considered, so the user can correct it before the perspectives are trusted.

Write the report itself in the language the user is conversing in — this
SKILL.md and the perspective files it produces are in English by convention
(they're read by review subagents), but the report is for the user.
