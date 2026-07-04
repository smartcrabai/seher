---
description: Use this when the user asks to capture lessons from this session as review perspectives, save review viewpoints, turn this session's mistakes/feedback into review checklist items, or add to ./.review/; it mines the current conversation for corrections, caught bugs, and non-obvious constraints, generalizes them into reusable checks, deduplicates against existing perspectives, and writes or updates one Markdown file per perspective under ./.review/ for the companion review-uncommitted and review-pr skills to consume automatically.
metadata:
    github-path: skills/improve-review-from-session
    github-ref: refs/heads/main
    github-repo: https://github.com/smartcrabai/agent-skills
    github-tree-sha: 5fdba0a928f27c16c586ee9b5462abc30fca5c98
name: improve-review-from-session
---
# Improve Review from Session

Turn what just went wrong (or almost went wrong) in this session into a
standing check that future reviews run automatically. The companion skills
`review-uncommitted` and `review-pr` load every file under `./.review/*.md`
as an additional lens on top of their generic reviewers. This skill is the
only way those files get populated, so the value of this whole system
compounds only if extraction actually happens after sessions that taught the
project something.

## Critical constraint: do this inline, never in a subagent

Perform the mining and writing yourself, as the main agent, in the current
turn. Do not delegate this task to a subagent.

The reason is structural, not stylistic: the source material is the
conversation history you are sitting in right now — the back-and-forth, the
corrections, the moment a bug surfaced after you thought you were done. A
freshly spawned subagent starts with none of that; it would have nothing to
mine and could only fabricate generic-sounding advice, which defeats the
purpose. Writing the resulting files is comparatively cheap and has no such
dependency, so it is fine to do inline too — there is no benefit to
offloading it.

## Step 1 — Mine the session

Scan the whole conversation, not just the last few turns, for review-worthy
lessons. Look in roughly this priority order, since earlier sources tend to
produce sharper, better-evidenced perspectives:

1. **Bugs or defects discovered after an implementation was believed
   complete.** For each one, ask: what check, run before declaring the work
   done, would have caught this? That check is the perspective.
2. **Corrections or pushback from the user** on the approach taken, the
   style used, or a requirement that was missed. The user correcting you is
   a direct signal of a gap between what you assumed and what the project
   actually needs.
3. **Findings from reviews run during the session** (e.g. a `/code-review`
   or `security-review` pass) that turned out to be valid — as opposed to
   ones that were investigated and dismissed as false positives.
4. **Non-obvious project constraints or domain rules** that surfaced along
   the way, e.g. "this API must never be called without X", "this config
   file is host-specific and templated differently per OS", "this table is
   append-only in production."
5. **Repeated friction** — anything the agent got wrong more than once in
   the same session. If it took two tries to get right, it will likely trip
   up a future session too.

## Step 2 — Generalize

A perspective is a reusable check for future, unrelated diffs, not a diary
entry about today's incident. Apply this test to every candidate:

> Would this check plausibly fire on a future diff that doesn't touch any of
> today's files?

If the answer is no — the lesson only makes sense with today's specific
variable names, file paths, or one-off typo — it is incident-specific
trivia, not a perspective. Drop it rather than forcing it into a file.

Hold the bar high. If nothing from the session survives this test, say so
and write nothing. A `./.review/` directory full of perspectives that never
fire, or that fire on everything because they were phrased too vaguely to be
checkable, trains the user to ignore review output entirely — that failure
mode is worse than an empty directory.

## Step 3 — Dedupe against existing perspectives

Before creating anything, read what is already there:

1. List `./.review/*.md`. If it does not exist, skip to Step 4.
2. For each existing file, read the frontmatter and the "When to apply"
   heading — that is enough to judge overlap without loading every file in
   full.
3. If a new lesson overlaps an existing perspective's territory, **update
   that file** instead of creating a near-duplicate:
   - Add the new concrete item to "What to check."
   - Append a sentence to "Background" noting this occurrence, so the file
     accumulates evidence over time instead of resetting.
   - Add or extend an example if the new case illustrates the check better.

Do this because duplicated perspectives dilute reviewer attention: the
parallel reviewers spawned by `review-uncommitted` and `review-pr` scale
with the number of perspective files, so two files saying almost the same
thing burns an extra subagent for zero extra coverage, and their
near-identical findings clutter the deduplication pass in those skills.

## Step 4 — Write perspective files

Create `./.review/` (repository root) if it does not already exist. Write
one file per genuinely new perspective, named `<kebab-case-id>.md` matching
the file's own `name` field.

Use this exact format — the companion review skills parse it, so keep the
section names and frontmatter keys stable:

```markdown
---
name: <kebab-case-id>
description: <one line — what this perspective checks>
origin: session <YYYY-MM-DD>
---

# <Title>

## When to apply
<which file types, code patterns, or change types this applies to — used by review skills to decide relevance>

## What to check
- <concrete, checkable items — imperative>

## Background
<the incident/lesson that motivated this perspective, 2-4 sentences>

## Examples
### Bad
<minimal illustrative snippet, optional>
### Good
<corrected version, optional>
```

Notes on filling it in:

- `description` and "When to apply" are read cheaply by other skills to
  decide relevance before loading the whole file — make them specific
  enough that an irrelevant diff (e.g. a CSS-only change) can be filtered
  out without a human or model having to read "What to check."
- "What to check" items must be concrete and checkable against a diff, not
  restatements of general good practice. "Handle errors properly" is not a
  perspective; "every call to `chezmoi apply` in a script must be preceded
  by `chezmoi diff` or an explicit `--force` flag" is.
- Use today's date for `origin`.

## Step 5 — Report

Report in the language the user has been conversing in throughout this
session — the skill file and perspective files are English-only by
convention, but the report is for the user, not the repository. Include:

- Each file created, with a one-line summary of the check it adds.
- Each existing file updated, with a one-line summary of what was added to
  it and why it was judged a match rather than a new file.
- Any candidate lesson that was deliberately dropped for being too
  incident-specific to generalize, so the user can override that judgment
  if they disagree.
- If nothing qualified, say plainly that no new perspectives were extracted
  this session, rather than staying silent or padding the report.
