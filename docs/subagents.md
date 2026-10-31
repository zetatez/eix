# Sub-agents

A sub-agent is a child session the main agent spawns to run one or more turns on its own in the
background. It has its own session id and its own tool-call history, and it hands a single result
back to the parent as a tool result.

Reach for one when the work is a well-scoped investigation or change that nobody needs to watch
while it runs.

## 1. Defining a sub-agent

A sub-agent is an ordinary agent file with `role: subagent`:

```markdown
---
name: test-writer
description: Add or repair tests for one module
role: subagent
model: claude-sonnet-4
---

You write tests. Read the module, find the untested branches, add focused tests,
and run them. Report the command you ran and its result.
```

Field semantics and override rules are in [Skills, commands, and agents](./skills.md). An agent
marked `subagent` never appears in the main agent picker; it can only be reached through
`spawn_agent`.

The built-in `explore` agent works without setup: read-only search and analysis that returns
evidence with `file:line` references. It does not edit files, run mutating commands, or ask
questions.

## 2. Spawning

The model calls the `spawn_agent` tool and names the agent in its `agent` argument. When no custom
agent fits, leave `agent` out to use the default configuration.

Several `spawn_agent` calls in one assistant message start concurrently. The ceiling is
`[tools] max_parallel_subagents` (defaults are documented in `config.tmpl.toml`). You can adjust
it temporarily from the TUI while a turn runs; the value must be one of 2, 4, 8, 16, 32, 64, 128,
or 256. A config reload or daemon restart returns it to the configured value.

## 3. Depth and concurrency

**Depth is limited to 1.** A sub-agent cannot spawn another sub-agent. The attempt returns an
explicit error instead of recursing.

**Concurrency follows the batch.** Within one batch of tool calls, `spawn_agent` always runs in
parallel. Other tools run in parallel only when they are classified as read-only; writes act as a
barrier. That barrier is what keeps a sub-agent from observing a half-applied working tree.

**Results are summarised, not streamed.** Each child session's result is appended to the parent as
one tool result, so the parent sees conclusions rather than the child's whole transcript. The
transcript stays in the child's event log and can be opened on its own.

## 4. Cost and observability

Every sub-agent is a separate model session, so its tokens and calls are billed separately.
Concurrency therefore sets peak cost: `max_parallel_subagents` is a performance knob and a budget
knob at the same time.

Child sessions are tagged as agent sessions, which means:

- They appear in the session list and can be inspected and replayed like any other session.
- They never raise approvals on their own; anything that needs a human decision is handled in the
  parent session.
- Cancelling the parent turn propagates to the children.

## 5. Writing a good sub-agent

- **One task per sub-agent.** "Find out why this test fails" works far better than "look at this
  repository". The narrower the task, the more usable the result.
- **Specify the output format in the prompt.** Ask for a conclusion, the evidence (`file:line`),
  and open questions, so the parent can act on the answer without another round trip.
- **State the boundaries.** For a read-only investigation, say explicitly that the agent must not
  edit files or run mutating commands, so it does not tidy the tree along the way.
- **Do not make it read a whole skill.** The parent loads skills on demand; pasting the relevant
  excerpt into the prompt costs less.

## 6. Troubleshooting

**"depth limit is 1".** The current session is already a sub-agent, so it cannot spawn another.
Move the task one level up, into the parent session.

**Concurrency does not go up.** Check whether `max_parallel_subagents` was reset by a config
reload. Adjustments made in the TUI are not persisted.

**Results are vague.** The task was probably too broad, or no evidence was requested. Narrow the
question and pin down the output format in the prompt.

**A child session hangs.** Cancelling the parent turn cancels it too. You can also open the child
session directly to see which step it stopped on.
