# Comparing two topologies on the same coding work

`/topology compare` executes one stated edit under two user-selected intents and
reports both outcomes side by side. It is an observation surface, not a ranking
or tournament.

## What varies

Each arm submits the same workspace state, exact edit, scope, and success check.
Only the requested intent differs. Intent changes the task profile supplied to
normal admission and selection; the comparison command does not choose a
registration directly or add a separate scoring rule.

## Isolation

Before either arm starts, the host creates two disposable copies of the source
tree. Each arm also receives its own single-use evidence store. Consequently:

- neither arm writes to the caller's workspace;
- one arm cannot observe the other's patch;
- one arm's outcome cannot change the other's selection;
- neither outcome changes later selection statistics in the live session.

The copies preserve the source tree's contents while excluding agent state and
build artifacts according to the comparison copier's rules.

## Reporting

Each arm reports the same result shape as a normal topology turn. Executed,
refused, and failed arms are all retained. The comparison report carries no
winner, aggregate score, or recommendation.

When an arm executes, its selection explanation is rendered from that arm's
recorded `SelectionDecision`, using the same projection as a single topology
run. A refused arm shows its admission or policy reasons instead.

## Verification

`tests/topology_comparison.rs` covers workspace isolation, evidence isolation,
refused and failed arms, report neutrality, and selection explanations.
