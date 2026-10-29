# Tool results

A tool result is read by one reader: the model deciding its next call. It is
not a log line and not a UI string. So each result answers three questions —
*what am I looking at*, *how much of it is this*, and *what do I call next* —
and answers them inside the same byte budget the result already had.

The budget matters as much as the wording. A result is prompt tokens on every
subsequent turn of the conversation, so a legibility gain paid for with extra
bytes is not a gain. Where these conventions add a line, they take the bytes
back from ceremony in the same result.

## Reads

`read_file`, `read_span` and the code-graph `read_symbol` all render through
`line_view` in [`tools/code.rs`](../crates/polaris_code/src/tools/code.rs).
The window is bounded by the requested range and by a byte budget, and neither
bound ends it mid-line: the last line shown is a whole line whose number is a
coordinate the model can resume from.

The line-number gutter is padded to the widest number *that window* can reach,
not to a fixed six columns. Alignment is all the padding buys, and every
unnecessary column is a byte charged on every line.

When a read stops short of the end of the file, the last line of the result
says so:

```
… showed lines 100-159 of 250. Next: read_file(path="src/run.rs", offset=160)
```

and when the byte cap rather than the range ended it:

```
… truncated at the output cap; showed lines 1-412 of 2101. Next: read_file(path="src/run.rs", offset=413)
```

Three properties are deliberate:

- **The totals are absolute.** `of 250` is what the model needs to decide
  whether the rest is worth fetching; "truncated" alone is not.
- **The resumption mechanism already exists.** The footer names `read_file`'s
  own `offset`, so no new tool, parameter or cursor is introduced to make the
  result actionable.
- **No footer means no more file.** Absence carries information, which is why
  the footer is never printed on a complete read.

`read_file` footers whenever the window stops short of EOF; `read_span` and
`read_symbol` footer only when the byte cap cut the view. A model that asked
for a bounded thing and got it does not need to be told the file is longer, and
would pay for the reminder on every symbol read.

An empty window still says what it looked at, because "nothing" is ambiguous
and "nothing, and here is why" is not:

```
(no lines: src/run.rs has 120 lines, and the requested range starts at 400)
```

## Failures

A failed operation names the thing it was operating on. `No such file or
directory (os error 2)` tells the model nothing it can act on; the operation
plus the path tells it whether to fix the path, create the file, or give up:

```
read_file 'src/run.rs' failed: No such file or directory (os error 2)
edit_file (write) 'src/run.rs' failed: Permission denied (os error 13)
old_string not found in src/run.rs (whitespace must match exactly); read the file and copy the text you mean to replace
old_string is not unique in src/run.rs (3 matches); add surrounding context to disambiguate, or set replace_all = true
```

The rejection messages end in the remedy rather than the diagnosis. The model's
next move is the point of the message.

## Searches

`search` names the query and the directory in both the empty and the capped
case, and a capped result says how to narrow rather than just that it stopped:

```
no matches for 'ToolRegistry' under 'crates'
… showed 200 of 200 matches (the walk stopped at the first 200); narrow 'query' or search a subdirectory of 'crates' to see the rest
```

## Shell

`run_command` belongs to upstream `polaris_shell` and returns a structured
`ShellToolResponse`. Serialising that document into the model's context spends
most of its bytes on escaped newlines and bookkeeping fields, and buries the
exit status in the middle. Presenting a result is the consumer's job, so
[`tools/shell_render.rs`](../crates/polaris_code/src/tools/shell_render.rs)
renders it at the point where tool values become tool-result blocks
(`execute_tools` in `agents/code_agent.rs`):

```
line one
line two
[stderr]
boom
[exit code: 3]
```

stdout is unlabelled — it is the common case and the bytes are better spent
elsewhere — and `[stderr]` introduces the error stream only when there is one.
The final marker line is always present and always last, so the status is never
something the model has to infer from the output:

| Situation | Marker |
|---|---|
| Exited | `[exit code: N]` |
| Exited 0 with no output | `[no output; exit code: 0]` |
| Timed out | `[timed out after 30s and was killed; rerun with a larger timeout_secs or a narrower command]` |
| Cancelled | `[cancelled before it exited]` |
| Killed | `[killed before it exited; no exit code]` |
| Refused by policy | `[not run: the shell policy requires confirmation for: <command>]` |

A truncated stream names its overflow path — `[stdout truncated at the shell's
output cap; all of it is in /tmp/…]` — so the model can read the rest with a
tool it already has.

Only the model's copy is rendered. The host activity block and the reified
`ToolCall` in the Session `ABox` keep the raw JSON, so the trajectory stays
structured and machine-queryable.

## Adding a tool

Reuse `line_view` / `resume_footer` / `cap_footer` for anything that shows part
of a file. For anything else, the convention is the shape, not a type: there is
no result envelope every tool must construct, and adding one would be a design
change rather than a convenience. Say what the result is, how much of the whole
it is, and which call gets the rest — in the fewest bytes that do it.
