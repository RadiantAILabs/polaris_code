# Rewind presentation

`polaris-tui` owns presentation, not rewind policy. A host advertises its own
slash command and emits `commands::RewindReport`, which carries independent
conversation, file, index and persistence outcomes plus explicit exclusions.
The conversation panel renders the report as ordinary transcript content.

This shape intentionally has no single success boolean: a file conflict can be
skipped while conversation restoration succeeds, and an index refresh can fail
without undoing restored bytes. Hosts must name partial, refused and stale
states instead of displaying “everything undone.” Checkpoint selection,
confirmation, quiescence, exact restore and edit-journal interpretation remain
host responsibilities.
