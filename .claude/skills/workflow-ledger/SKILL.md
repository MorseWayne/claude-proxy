---
name: workflow-ledger
description: Lightweight, recoverable development workflow for Claude Code. Use when starting, resuming, planning, tracking, reviewing, or closing multi-step code work; when the user wants traceability without heavyweight specs; or when a task may be interrupted and continued later.
when_to_use: Use for development tasks that need phased progress, dependencies, acceptance/review summaries, or cross-session recovery. Skip for pure Q&A and trivial one-step edits unless the user requests tracking. Trigger phrases include "start task", "resume task", "track progress", "workflow", "ledger", "recover", "continue previous task", "what is left", "review progress".
argument-hint: start|resume|update|close [task]
---

# Workflow Ledger

Use this skill to keep development work traceable and recoverable while staying lightweight.

## Project installation contract

Recommended one-command project setup from the target project root:

```bash
curl -fsSL https://raw.githubusercontent.com/MorseWayne/workflow-ledger/main/install.sh | bash
```

For project-local use, the installer configures all three pieces:

1. Copy this skill to `.claude/skills/workflow-ledger`.
2. Add [examples/claude-project/CLAUDE.md.snippet](../../examples/claude-project/CLAUDE.md.snippet) to the project's `CLAUDE.md` if missing.
3. Create `.claude/WORKFLOW.md` from [templates/WORKFLOW.md](templates/WORKFLOW.md) if missing.

The `CLAUDE.md` snippet is important because skill auto-loading is not guaranteed. Keep mandatory reminders short in `CLAUDE.md`, and keep detailed procedure here in the skill.

## Core rule

Maintain one project overview file at `.claude/WORKFLOW.md` for Level 2/3 work and for any task the user wants tracked. Do not create per-task files by default.

Use attachments only when needed for long research, large impact reports, or Level 3 design details. Link attachments from `.claude/WORKFLOW.md`.

## Workflow levels

Classify first. If uncertain, choose the lighter level unless risk appears.

- **Level 0 — Q&A / read-only**: explain, answer, inspect without edits. No ledger required.
- **Level 1 — lightweight edit**: typo, docs tweak, formatting, tiny config, no runtime behavior change. Ledger optional.
- **Level 2 — standard code work**: small bugfix, single-module behavior change, tests, provider logic, repeatable multi-step work. Use ledger.
- **Level 3 — complex work**: new feature, cross-module or cross-repo changes, public API/data model changes, auth/streaming/concurrency/metrics, unclear requirements, high-risk impact. Use ledger and consider a design/plan attachment only if needed.

Escalate when you discover cross-file behavior changes, public API changes, failed validation, unclear requirements, or HIGH/CRITICAL impact.


## Anti-rationalization rules

Do not skip ledger work by rationalizing that the current change is small or almost done.

| Rationalization | Required behavior |
|---|---|
| This is too small for the ledger | Classify first; Level 2/3 must be tracked |
| I will update the ledger at the end | Update at phase completion, blockers, key decisions, and handoff points |
| Tests passed, so the phase is done | Record validation evidence and remaining gaps before marking Done |
| TodoWrite is enough | TodoWrite is session-local; the ledger is cross-session state |
| Formatting does not matter | Preserve stable fields so `doctor` can check the ledger |

## Ledger structure

`.claude/WORKFLOW.md` should contain:

1. `Active` — current task entries, each with phases.
2. `Backlog / Future` — discovered tasks not needed for the current goal.
3. `Completed` — milestone summaries and commits.

Each active task should have:

- stable ID: `WF-YYYY-MM-DD-NNN`
- status, level, current phase, start/update dates
- goal
- decisions
- phases with subtasks
- dependencies
- discovered tasks
- resume next

## Phase structure

Use phases, not a flat checklist.

Each phase should include:

```markdown
#### Phase N — Name
Status: Pending | In Progress | Blocked | Done
Depends on:
- None
Tasks:
- [ ] Task

Acceptance / Review:
- Review: N/A
- Validation: N/A
- GitNexus: N/A
- Tests: N/A
- Gaps: N/A
```

Only completed phases need `Acceptance / Review`. For blocked phases, write `Blocked by` and `Resume next` instead.

## Acceptance / Review

Before a phase is marked Done, record a short summary directly under that phase. A phase with failed validation must remain `In Progress` or `Blocked`.

- **Review**: what was reviewed or confirmed.
- **Validation**: commands/checks/manual validation performed.
- **GitNexus**: impact/detect/query results, or why not applicable.
- **Tests**: tests run, exact commands, or why omitted.
- **Gaps**: explicitly `None` or known unresolved items, deferred work, or risks.

Keep it summary-level. Do not write a transcript. `Validation` and `Tests` must name actual checks or explain why they do not apply.

## Dependency and discovered-task rules

When a new prerequisite appears:

1. Add it to the current phase `Depends on` or create a preceding phase.
2. Mark the blocked phase as `Blocked` if work cannot continue.
3. Complete the prerequisite before continuing.

When a new task is useful but not required for the current goal:

1. Add it to `Backlog / Future` or the task's `Discovered tasks`.
2. Do not insert it into the current phase unless it blocks completion.
3. Note the decision briefly.

## Start a tracked task

1. Locate or create `.claude/WORKFLOW.md` using [templates/WORKFLOW.md](templates/WORKFLOW.md).
2. Create one `Active` task entry.
3. Classify Level 0-3.
4. Define 2-5 phases.
5. Expand only the current phase into concrete subtasks.
6. Set `Current phase` and `Resume next`.

## Resume a task

1. Read `.claude/WORKFLOW.md`.
2. Find `Active` tasks.
3. Use `Current phase`, unchecked tasks, dependencies, and `Resume next` to continue.
4. Verify current repo state before trusting stale ledger details.
5. If code state differs from the ledger, update the ledger with the observed state.

## Update during execution

Update the ledger only at milestone points:

- task start
- key decision
- phase completion
- dependency/blocker discovered
- validation result
- interruption handoff
- commit/close

Do not update it after every small edit.

## Close a task

1. Ensure all required phases are Done or explicitly deferred.
2. Add acceptance summary to the final phase.
3. Move the task from `Active` to `Completed`.
4. Include commits, validation summary, gaps, and follow-up tasks.
5. Keep future work in `Backlog / Future`.

## Interaction with other mechanisms

- Use TodoWrite for current-session execution tracking.
- Use `.claude/WORKFLOW.md` for cross-session recovery and milestone history.
- Use CLAUDE.md for short mandatory project rules.
- Use hooks only for hard guardrails such as blocking unsafe commits.
- Use heavier design/spec artifacts only for Level 3 work that genuinely needs them.
