# Project instructions

`polaris-code` grounds every turn with the project's own instruction documents:
`AGENTS.md`, or `CLAUDE.md` where a directory has no `AGENTS.md`. They are
rendered into the system prompt's `<project-instructions>` block by
`ProjectContext` (`src/agents/project_context.rs`).

## Where they are found

Discovery starts at the working directory and walks **up** through its
ancestors, reading at most one document per directory (`AGENTS.md` wins over
`CLAUDE.md`; an empty file is not a document). So a monorepo whose conventions
live at the repository root still grounds an agent started in one of its
packages.

The walk stops at a boundary rather than at the filesystem root, because above a
checkout sits whatever happens to be there — `/tmp/AGENTS.md`, or a neighbouring
project in a multi-repo workspace:

| Boundary | When it applies |
|---|---|
| The **repository root** — the nearest ancestor (working directory included) holding a `.git` entry | Whenever the working directory is inside a repository. This is the normal case. |
| The user's **home directory** | Only when no repository root is on the chain and home is an ancestor. |
| The **working directory alone** | When neither boundary is on the chain — the behaviour before the walk existed. |

A repository root is where a project states its own conventions, and everything
above it belongs to somebody else's project. `.git` being a *file* rather than a
directory — a worktree or a submodule — counts the same.

There is no user-global or organisation-scope layer, and no directory-scoped
document below the working directory. Both are separate features, not
consequences of this one.

## Precedence

Documents are rendered **nearest first**, each tagged with the file it came
from, and the block states the rule so the model applies it rather than
inferring it from order:

```text
<project-instructions>
The project's instruction documents, nearest first. Where two of them conflict,
the earlier one — the one closest to the working directory — wins.

<document path="/repo/crates/inner/AGENTS.md">
…
</document>

<document path="/repo/AGENTS.md">
…
</document>
</project-instructions>
```

## The size budget

`DOC_CAP` (8,000 characters) is the budget for **all** documents together, spent
nearest first:

- The nearest document is always rendered. If it alone exceeds the budget it is
  truncated at a char boundary with the visible `… (truncated)` marker — exactly
  what a single over-long `AGENTS.md` got before the chain existed, except that
  the user is now told, because a rule cut in half reads like a whole rule.
- A farther document that no longer fits **whole** is dropped whole, and so is
  every document beyond it. Half a rule reads like a whole rule, and trimming a
  nearer document to make room for a farther one would invert the precedence the
  chain exists to express.
- Every dropped document is named — in the block, as
  `<omitted path="…" reason="…" />`, and to the user (see below).

## Freshness

The documents are re-read at **every turn boundary**, by the agent's
`refresh_project_docs` system, which runs after `receive_user_input` and before
the loop. A document edited mid-session — to correct an agent that is getting
something wrong — therefore reaches the model on the next turn rather than after
a restart. A removed document stops being projected on the same schedule.

Only the documents refresh. The rest of `ProjectContext` — working directory,
date, git branch, `git status --short`, recent commits — is still captured once,
in `CodeAgent::setup`. That asymmetry is a cache decision:

> The grounding block is the head of the prompt prefix a provider caches on
> (`CacheAwareWindow`, `src/agents/context.rs`). `git status --short` moves on
> nearly every turn of a coding session, so refreshing it here would spend a
> cache miss per turn to report something nobody edited. The documents move only
> when a person edits one, so the miss is paid exactly on the turns that earned
> it.

Change detection compares document **content**, not mtime, so two edits inside
one filesystem timestamp tick cannot be missed. On a turn where nothing changed,
the refresh mutates nothing and the projected system prompt is byte-identical to
the previous turn's.

Refreshing git state as well remains an open question — see
`docs/known-issues.md`.

## What the user is told

Instructions read from above the directory the user chose to work in are never
honoured invisibly. `ProjectContext::take_notice` hands the front end one line,
and `refresh_project_docs` sends it as a system message:

| When | Line |
|---|---|
| First turn, if any document comes from outside the working directory, or the budget cut or dropped one | `Project instructions in force (nearest first): …` |
| Any turn where the set changed | `Project instructions reloaded (nearest first): …` |
| A turn where the last document was removed | `Project instructions reloaded: none found` |
| A single document inside the working directory that fits, unchanged | *nothing* — it is what the user asked for |

A truncated document is annotated in place —
`/repo/AGENTS.md (truncated at the 8000-character budget)` — and dropped ones
are appended as `; omitted, over the 8000-character budget: …`.

## Tests

- `src/agents/project_context.rs` (unit): the ceiling, the chain, `AGENTS.md`
  over `CLAUDE.md`, precedence, the budget's drop-whole and truncate-nearest
  arms, and the notice rules.
- `tests/project_grounding.rs` (integration): the same claims asserted on the
  `system` string a capturing `LlmProvider` is actually handed, across two real
  turns — including the stability counterweight, which edits an ordinary source
  file between turns so that a naive "re-discover everything" refresh fails it.
