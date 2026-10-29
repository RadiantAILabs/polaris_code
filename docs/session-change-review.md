# Session change review (`/diff`)

`/diff` answers "what has this session changed, and what was the change?" in
one transcript message, without scrolling back through per-message diffs.
`/diff <path>` renders one file at a larger budget. Both are read-only:
reviewing never writes to a file, the journal, or the session.

It composes three things that already existed — the additive
`CommandRegistry`, the tracked-edit journal from
[session rewind](session-rewind.md), and the conversation panel's existing
unified-diff colouring for a ```` ```diff ```` fence. It adds no panel, no
diff engine and no change-tracking service.

## What the change is measured against

- **Baseline:** the bytes each file held when *this session* first edited it —
  the `before` image of the first tracked edit at or after the session's
  earliest retained rewind boundary. A boundary is captured before every
  prompt, including the first, so this is where the session began.
- **Current:** the bytes on disk now, not the journal's last after-image.

Reading disk rather than trusting the journal's own record is what keeps the
view honest: after a `/rewind` the restored file correctly drops out of the
change set, and a file something else wrote afterwards is flagged rather than
attributed to the agent.

## What it cannot see, and how it says so

The journal only records successful `edit_file` whole-file writes. Shell
writes, editor and external edits, and files created or deleted outside
`edit_file` are not tracked. Every reply — including the empty one — carries a
standing note saying that and pointing at `git status` for the whole working
tree. Four narrower cases are called out where they apply:

| Case | What the view says |
|---|---|
| A tracked file that something else also wrote | `⚠ also written outside \`edit_file\`` on that row, and the diff shows the disk state |
| A tracked file that was deleted | `deleted — outside \`edit_file\`, not restorable` |
| Journal retention evicted this session's earliest edits | a warning that some baselines are later than the session's first edit, so those changes are understated |
| No rewind boundary for this session survived | a warning that no session-scoped change set could be built |

A file `edit_file` never touched is invisible to this view and cannot be
detected at all — which is exactly why the standing note is unconditional.

## Bounds

A large change set degrades by declared constants rather than emergently
(`terminal::review`): at most 100 summary rows, inline diffs for at most 20
files at 120 diff lines each, and every rendered line clipped to 500
characters. `/diff <path>` renders one file at 600 diff lines. Anything
elided says how much and names the command that shows it.

## Out of scope

This is the report half only. There is no accept, reject, or per-hunk action
here, and no edit-application policy is implied by it.
