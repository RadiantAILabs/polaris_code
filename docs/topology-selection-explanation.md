# Explaining which topology ran, and why

An executed topology result explains the recorded selection without running
selection a second time. Both the terminal panel and headless JSON read the
same `ExecutedTopology` and `SelectionDecision`.

## What a user sees

The panel adds:

- **why this one** — the decision's `SelectionRationale`, using its display
  summary when present and otherwise its decisive score components;
- **alternatives considered** — every other candidate recorded by the
  decision, shown with its rank or its own exclusion codes.

The headless JSON report carries the same facts under its `topology` member as
structured values suitable for scripts.

No renderer recomputes candidate facts, admission, scores, ranks, or exclusion
reasons. This keeps the explanation tied to the decision that actually drove
execution, even if host state changes later.

## Quiet when no topology ran

Conversational turns and no-start topology results do not get an empty
selection explanation. Their existing result or refusal text already explains
what happened.

## Verification

`tests/topology_ux_interaction.rs` and `tests/topology_turn_answer.rs` cover
how a selection and a no-candidate answer are explained.
