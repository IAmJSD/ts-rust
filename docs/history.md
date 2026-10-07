# How ts-rust got here

This text was in the README before the first preview release (2026-10).

## The model experiment

This repository is also an experiment in how capable `gpt-5.6-sol` is at
understanding, validating, and completing a compiler-sized systems port through
Codex and its subagents.

There is an important provenance caveat: the archived Codex log labels 236 of
the original implementation turns as an earlier model, one later turn as
`gpt-5.5`, and only the brief July 8 resume as `gpt-5.6-sol`. The existing code
is therefore the starting artifact for the `gpt-5.6-sol` experiment, not
evidence that every existing line was produced by that model name.

## The first goal runs (June to July 2026)

The implementation logs show three distinct Codex goal runs:

| Goal run | Outcome | Goal-accounted tokens |
| --- | --- | ---: |
| Full Rust port, June 22–26 | Produced the broad prototype. A later audit found 70 failing tests and measured the broad compiler 2.6–3.4x slower than Go for equivalent work. Paused. | 207,170,354 |
| `minimal-working-v0`, June 26 | Narrowed the claim to the explicit five-file no-check contract and built its oracle, runtime, and benchmark gates. Completed. | 1,439,471 |
| Type-checking parity, June 27–July 8 | Added substantial checker work, but never demonstrated full `tsgo` parity. Paused. | 7,438,681 |
| **Total** |  | **216,048,506 (~216 million)** |

The estimate uses Codex's goal-level `tokensUsed` counters for the three runs
that were explicitly tied to this repository. Those counters equal uncached
input plus output tokens. The raw session log records about 8.34 billion
input-plus-output tokens, but about 8.12 billion of those are cached context
replay, so that larger number is not a useful estimate of new inference spent
on the project. Work between goals and this README update are not included.

The history also explains why breadth is not the same as completion here. The
first goal landed hundreds of commits across many compiler subsystems before
the parity harness was authoritative. A recovery audit then reduced the scope
to one measurable path. The later checker goal expanded the scope again and
was suspended before cleanup and full verification were complete.
