# Terminal session rewind

Interactive `polaris-code` captures a native Polaris checkpoint before each
input and couples it to a bounded, workspace-local tracked-edit cursor. Use
`/rewind` to list retained points, `/rewind <point> <mode>` to preview affected
paths, and repeat the displayed command with `confirm` to apply it at the next
quiescent host boundary.

Modes are `conversation`, `edits`, and `both` (the default). `both` restores
eligible files first and then calls `SessionCheckpoints::restore_exact`; exact
restore was chosen deliberately because it validates serializers before any
context mutation and therefore cannot leave the session incoherent after an
unmatched resource. The original state remains available through the native
pre-restore checkpoint and the resulting session/checkpoints are saved.

Only successful `edit_file` and `create_file` whole-file images are tracked.
A `create_file` creation is journaled with an empty pre-image — the exact
bytes the path held before, which were none — so `/rewind` lists the created
path and a rewind past the creation takes the file back to zero bytes. It does
not remove the file: the journal restores images and never deletes, and an
empty file is the honest limit of what an image-only journal can promise.
Exact bytes and the entire intervening chain must still match. External
divergence, chain gaps, missing files, symlinks, hardlinks, oversized or
evicted images are skipped, never overwritten. Shell commands, external edits,
creates and deletes made by anything other than those two tools, build
artifacts, memory, knowledge history and remote side effects are explicitly
outside the boundary. The journal lives at `.polaris/rewind-v1.json`, is
atomically replaced, and is bounded to 64 MiB, 50 edit groups and 8 MiB per
image.

Every result reports conversation, files, index and persistence independently.
A hard file replacement failure stops later file writes and prevents the
conversation restore; a skipped conflict may coexist with a successful
conversation restore. Restored or uncertain paths are re-indexed, and index
failure is shown as stale rather than silently called fresh.
