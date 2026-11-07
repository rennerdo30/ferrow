# ADR 0005 review 1 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
`ed01ded`; source and Microsoft documentation only. Verdict: **reject the
current proposal.**

Required changes (P1 unless noted):

1. **USN cannot give strict revocation freshness.** Journal batching leaves a
   window after a revocation, and NTFS may suppress repeated reasons until
   close. Strict post-revocation guarantees need fresh, object-bound
   authorization checks at query time (or an equivalent); filtered callers stay
   unavailable whenever freshness is unknown.
2. **Objects vs. names.** One FRN can have several hard-link names in different
   folders; model file identity one-to-many with name entries; validate file and
   parent sequence numbers; ReFS ids stay opaque 128-bit values.
3. **Delta lifecycle.** Define delta-entry delete/rename, repeated directory
   moves, canonical parent resolution used by every path consumer, case-only
   renames (raw bytes change).
4. **Subtrees across boundaries.** Moving an unindexed tree into a root (or an
   indexed tree into an exclude) is one rename record; reconcile or suppress the
   whole subtree.
5. **Initial walk consistency.** A move between visited and unvisited folders
   during the walk loses or duplicates a subtree; needs idempotent
   reconciliation with a catch-up boundary.
6. **Re-freeze protocol.** One publish owner; replay identity-based operations
   through a boundary; no lost-update window.
7. **Metadata overrides break the base sort order.** Overridden entries must
   leave the base order and enter an ordered overlay (keep ADR 0003's
   comparator).
8. **One effective view** for search, count, prefilter, paths, cancellation
   prefixes and sort readiness.
9. **Persist the full applied state** with the next-read USN; crash-point tests.
10. **Continuous per-volume failure states** (catching up, live, unavailable,
    rescanning); walk-only volumes cannot serve filtered callers from cached
    descriptors.
11. **Win32 contract.** `OpenFileById` is volume-relative, not parent-relative;
    define buffers, USN V2/V3 parsing, `ReturnOnlyOnClose=0`, `CancelIoEx`
    completion handling.
12. **Volume identity and mount topology.**
13. **Bounded background work** isolated from the query pool; bounded queues
    and snapshot reclamation.
14. (P2) **Memory and absolute performance targets** (SPEC §3), including
    re-freeze peaks and open files whose size changes are reported only on close.

Suggestion: stable directory/object handles plus separate name-edge ids;
compare handle enumeration with `FSCTL_ENUM_USN_DATA` on hosted CI.
