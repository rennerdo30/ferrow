# ADR 0002 security review 3 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
revision 3 at `75de409`. Verdict: **accept with required changes**; keep the
ordinary-caller gate closed until they are specified and the owner accepts the
corrected residual risks.

Resolved as requirements: review 1 items 2, 3, 9, 10; review 2 items 2, 3
(in scope), 4, 5, 6. Scoped out: review 1 items 5, 6 (Unix). Partial or open:
the five below.

1. **Indexed metadata must not decide membership.** `report.dat size:<T`
   returns a file only when both its old indexed and its fresh size match, so
   sweeping T recovers the old size. Candidate generation must treat mutable
   metadata and file type as unknown and evaluate the whole expression on
   fresh values (or scope those predicates out for filtered callers).
2. **Concurrency contract.** Retained ancestor handles allow a request to
   return a path that never existed during it (a folder moved under a private
   parent between opens). Either keep the chain attached (ancestor handles
   without delete sharing) or accept request-scoped retained-handle semantics;
   also define root rows, case-sensitive names, 8.3 aliases, ADS.
3. **Traverse.** Request list *and* traverse on every directory used for
   descent; relative opens do not by themselves re-check traverse.
4. **Residual risks are broader than stated.** An adaptive existence oracle
   (extend a prefix one character at a time; narrow with `path:`/`parent:`)
   can reconstruct hidden names, paths and indexed properties; ordering
   allows inference of old metadata. Either remove the signals or have the
   owner accept that broader disclosure explicitly.
5. **Resources and scheduling.** Bound memo handles, batches, retained
   snapshots, per-SID and per-volume outstanding jobs (charged until they
   finish); pre-handshake admission; isolate scan I/O from the query pool;
   teardown never joins a stalled syscall; no late output.

Notes: a hard link under a genuinely authorised parent is discoverable there;
a retained process handle prevents id reuse, not exit; exact broad counts are
not achievable (5 M rows in 3 s on 4 workers is 2.4 µs per row), so counts are
lower bounds; the 16 ms first-page goal needs its own hosted-CI gate.
