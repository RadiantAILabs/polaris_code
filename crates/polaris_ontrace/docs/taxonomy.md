---
title: polaris_ontrace Taxonomy
---

# polaris_ontrace Taxonomy

This document defines the conceptual layers and core abstractions in
`polaris_ontrace`. For the design philosophy these layers serve, see the
shared, single-source [philosophy.md](./philosophy.md) (canonical copy in the
[Polaris repository](https://github.com/RadiantAILabs/polaris/blob/main/docs/philosophy.md)).

`polaris_ontrace` is not a framework — it is a collection of `polaris::Plugin`s
that turn a trace stream into a temporal knowledge graph. So its layers are not
Polaris's L1/L2/L3 framework layers; they are *this crate's own* layers,
classified by the same rule: **lower layers are fixed contracts, upper layers
are swappable parts that compose across those contracts.**

## Architecture

| Layer | Name | Owns |
|-------|------|------|
| **3** | Knowledge Surfaces & Vocabulary | curated ontology / seed packs · SPARQL toolset · inspection front-ends (CLI, TUI panels, `InspectAPI`) |
| **2** | Pipeline | sources/transports · ingest dispatcher · extraction backends + the per-trace extraction agent |
| **1** | Contracts & Data Model | trace-ingest contract · `KnowledgeGraph` triple model · IRI scheme |

The whole crate is organized around one composability seam — the
`TraceIngestAPI` / `TraceIngestRx` pair in Layer 1 — that lets any set of
Layer-2 sources fan in to exactly one Layer-2 backend without either side
naming the other.

## Layer 1: Contracts & Data Model

**Files:** `src/plugins/trace_ingest.rs` (the API/Rx contract), `src/resources/kg.rs`,
`src/resources/trace_line.rs`, `src/iri.rs`, `src/dump.rs`.

The fixed primitives every other part of the crate agrees on:

- **The trace-ingest contract.** `IOMessage` → `TraceLine`, and the capability
  seam `TraceIngestAPI` (sources push) / `TraceIngestRx` (one backend `take()`s
  the receiver). `TraceIngestPlugin` `provides` both; sources `requires`
  `TraceIngestAPI`, the backend `requires` `TraceIngestRx`. Neither side names
  the other — they depend only on this contract.
- **The triple data model.** `Triple`, `Entity`, `Predicate`, and the
  append-with-timestamp semantics of `KnowledgeGraph`.
- **The IRI scheme.** `urn:ontrace:e/…` / `urn:ontrace:p/…` minting and the
  Turtle serializer.

These define *what a trace and a fact are* in ontrace. Changing them is a
breaking change to every source, backend, and tool. **Scope:** Layer 1 holds no
transport logic, no LLM calls, no store implementation, and no vocabulary — only
the contracts those upper layers meet on.

## Layer 2: Pipeline

**Files:** `src/sources/**` (transports), `src/plugins/trace_ingest.rs`
(dispatcher), `src/plugins/extraction.rs`, `src/plugins/oxigraph_extraction.rs`,
`src/agents/**`.

The swappable parts that move a trace from ingress to a written fact, composed
across the Layer-1 seam:

- **Sources / transports.** One plugin per transport (`StdinIOPlugin`,
  `FileTailIOPlugin`, `OtlpIOPlugin`, `ClaudeCodeIOPlugin`), each an
  `IOProvider` that pushes into `TraceIngestAPI`. Mix as many as you like.
- **Ingest dispatcher.** `TraceIngestPlugin` owns the mpsc channel and knows
  nothing about agents or backends.
- **Extraction backends + agent.** Exactly one backend claims `TraceIngestRx`
  and drains it (`ExtractionPlugin` over the naive `KnowledgeGraph`, or
  `OxigraphExtractionPlugin` over `OxigraphStore`). Both drive the *same*
  per-trace, stateless `extract_triples` → `write_to_kg` two-system graph,
  shaped by a `Res<OntologyContext>`.

**Scope.** Sources must stay indifferent to which backend is wired up (depend on
the contract, not a backend). At most one backend may claim the receiver. The
extraction agent is stateless across traces — durable state lives only in a
Layer-1/Layer-3 resource, never in agent fields.

## Layer 3: Knowledge Surfaces & Vocabulary

**Files:** `ontologies/*.ttl` + `src/agents/seed.rs`, `src/oxigraph/**`
(`OxigraphStore`, `OxigraphTools` SPARQL toolset), `src/tui/**`
(`InspectTuiPlugin`, panels), `src/bin/ontrace.rs`.

Everything optional and replaceable that sits *over* the accumulated graph:

- **Vocabulary.** The curated ontology / seed packs that shape extraction. The
  vocabulary is data, never baked into the library — loaders take a path.
- **Query surface.** The SPARQL toolset over the shared store.
- **Inspection front-ends.** The `ontrace` CLI and the polaris-tui inspect
  panels (`InspectAPI`, chat, `SparqlPanel`). Optional; the pipeline runs
  headless without them.

**Scope.** A consumer may omit every Layer-3 surface and still ingest traces
into a graph; or swap the store, the vocabulary, or the front-end independently.

## Boundary Guide

When adding functionality, place it by asking:

| Question | If yes | Examples |
|----------|--------|----------|
| Does it change what a trace or a fact *is*, or the ingest seam? | **Layer 1** | New field on `TraceLine`, a new `TraceIngest*` capability, a change to the triple model or IRI scheme |
| Is it a new way to get traces in, dispatch them, or extract/store them? | **Layer 2** | A new source transport, a second backend, a change to the extraction graph |
| Is it vocabulary, a query surface, or a way to inspect the graph? | **Layer 3** | A new seed pack, a SPARQL tool, a TUI panel, a CLI subcommand |

Lower layers must not depend on upper layers: Layer 1 names no transport,
backend, or vocabulary; a Layer-2 source must not reach into a backend's
internals; Layer-3 surfaces depend on the store and contracts, not the reverse.
