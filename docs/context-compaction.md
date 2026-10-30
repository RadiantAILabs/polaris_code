# Context compaction

The coding agent keeps the full canonical conversation in `ContextManager`.
Compaction changes the window sent to the model, not the transcript or the
recorded tool results.

`CodeAgentPlugin` selects `SummarizingWindow` around the existing cache-aware
strategy, or around the sliding strategy when prompt caching is disabled.
Embedders can select another `ContextStrategy`; strategies without summary
support continue to project without making summary calls.

With prompt caching enabled, old text tool results are first replaced by
deterministic elision stubs once the estimated context exceeds 60,000 tokens.
If the remaining history exceeds 90,000 estimated tokens, the graph asks the
selected model for a structured checkpoint containing:

- the goal and constraints;
- decisions and their reasons;
- files touched;
- open tasks and unresolved checks;
- facts needed to continue.

The 50-message recency cap alone does not trigger a paid summary.
`PolicySnapshot.retrieval.summarize_after_tokens` controls the automatic token
threshold; setting it to `None` disables automatic summaries.
`summary_output_tokens` limits each summary response to 2,048
tokens by default; it cannot be zero.

The summary covers older messages and preserves at least the configured
`compact_keep_recent` messages (12 by default). It never splits a tool call
from its result. It can include complete older turns and completed tool
exchanges from the older portion of a long active turn.
Each new summary incorporates the previous checkpoint and
the newly covered history. The checkpoint stays at the start of subsequent
projections even when their recent-history cap slides. Existing cache
breakpoints are recomputed over that window, and checkpoint bytes stay stable
until another summary runs.

Use `/compact` to request a checkpoint immediately, or
`/compact preserve the failing tests and next steps` to give it a focus.
`CompactionPlugin` contributes the command, and it requires `CodeAgentPlugin`,
which reads the request: a host that runs a different agent leaves the plugin
out and never advertises a command nothing serves. The command hands the agent
a typed `AgentRequest`; the generic command registry ships only `/help` and
knows nothing about compaction. The command does not append a user prompt and
does not ask for a normal assistant answer. Its summary call still uses the selected session model.
When there is no eligible older span, the projection stays as it was.
The session status row records `summarized at turn N` after a successful call.

Every summary reserves its request and output allowance through `TokenMeter`
and `CostPermit`, then bills provider-native usage, including cache tiers.
Summary requests use the same streaming provider interface as normal answers.
Usage received before an interruption or stream failure is still billed.
A request that is cancelled or abandoned before it reports usage stays charged
at its reservation estimate, because the provider may still bill it, so
`--max-cost-usd` cannot under-count a summary the user interrupted.
A summary call that goes quiet does not hold the turn: opening the stream and
every gap between its events are bounded by `RetryPolicy.attempt_timeout`
(300 seconds by default), the limit the normal model call runs under, and an
expiry is a failed summary like any other. An unattended run therefore always
reaches its answer.
Failed or malformed summaries leave the existing projection in place, with
cheap elision as the cache-aware fallback. A billed response is charged even
when its summary is rejected. Automatic failures are not retried for the same
older span; an explicit `/compact` can retry.
An explicit focus also rereads the canonical older span, so details omitted
from a previous checkpoint can be recovered in the new summary.

Checkpoints are serialized alongside canonical history. Native resume and
conversation rewind restore both together; restoring a state before a
checkpoint restores the earlier projection. Runtime token calibration and
pending commands are reset on restore.

Summaries are model-produced working memory and may omit details. The full
history remains available for review. Protected recent messages or an
unfinished tool exchange can still exceed the provider's context limit.
An older span that remains over its input budget
after elision also falls back without a summary call.
