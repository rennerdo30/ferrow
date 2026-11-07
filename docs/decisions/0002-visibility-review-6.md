# ADR 0002 security review 6 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
revision 6 at `178b00d`. Verdict: **accept with required changes**; gaps G1
and G2 stay accepted. Review-5 item 1 (ancestor swap) resolved for ancestor
binding; no disclosure bypass found via hard links, reparse points, mount
points, POSIX rename/delete, ReFS or case-sensitive directories. The
pre-filter's denial-only argument holds.

1. (P1) **A blocked open can retain the chain indefinitely.** An attacker
   holding a descendant directory with `DELETE` access and a Read-Handle
   oplock can withhold the break acknowledgement; the sharing-violation
   break waits with no timeout, so ancestors already opened without delete
   sharing stay held. Fix: never wait while holding (e.g.
   `FILE_COMPLETE_IF_OPLOCKED`, `STATUS_OPLOCK_BREAK_IN_PROGRESS` → hidden,
   close any returned handle); define how held handles are released when
   another filesystem call stalls; if a hard 5 ms bound cannot be
   established, get an explicit decision on the real bound. Probes: a
   withheld acknowledgement in CI and a mocked stall after ancestors are
   held.
2. (P1) **Bounded holds can still prevent renames indefinitely.** Two jobs
   or requests overlapping holds on the same ancestor keep it continuously
   locked although each hold ends within 5 ms. Fix: bound exclusion across
   workers and requests with coordinated release and a backoff that gives
   other programs a chance; document the bound; add an overlapping-holds
   probe with a competing rename. "Programs that retry succeed" was
   unsupported.

Applied in revision 7: rule 4 (never wait while holding) and the per-volume
"Hold windows" gate (≤ 10 ms windows, ≥ 20 ms free gaps, forced end with
`CancelSynchronousIo`; only device or driver stalls can extend a hold).
