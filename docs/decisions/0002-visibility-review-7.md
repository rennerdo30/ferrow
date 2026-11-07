# ADR 0002 security review 7 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
revision 7 at `2ca7a43`. Verdict: **accept**. No remaining blocker under the
stated threat model with the owner-accepted gaps G1 and G2 and the
hold-window availability bound.

- Review-6 item 1 resolved: `FILE_COMPLETE_IF_OPLOCKED` with
  `STATUS_OPLOCK_BREAK_IN_PROGRESS` → hidden covers create-time oplock
  breaks generally (including Batch/Filter breaks before sharing checks);
  the service never awaits a break. Filters must honour the flag or fail
  with a sharing violation; misbehaving drivers fall under the accepted
  stall exception.
- Review-6 item 2 resolved: one gate per volume across requests, users and
  workers; the gap starts only after all holds have drained; handle closing
  stays with the owning job.
- Byte-range locks, directory change notifications, metadata queries and
  cleanup add no acknowledgement wait to the open/query/close sequence.
- No disclosure outside G1/G2 found.

Suggestions (added to the ADR's tests): probes for other oplock types and
atomic create-with-oplock; window expiry racing with system-call entry and
return; trace actual handle release and the full free gap afterwards
(`CancelSynchronousIo` returning does not prove completion).

Document and primary-source review only; no runtime timing proof.
