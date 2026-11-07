# ADR 0002 security review 2 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
revision 2 at `09e883e`. Verdict: **accept with required changes; not safe to
implement unchanged; keep the ordinary-caller gate closed.**

Required changes (P1 unless noted):

1. **Historical names.** A current permission on a folder does not authorise
   names indexed in it earlier: move a secret file out, then grant B listing on
   the folder, and B still sees the old name. Each returned row must be
   validated as a current child through the authorised parent handle (with its
   metadata), or an equally strong freshness mechanism must exist.
2. **Scan binding.** Capture identity, reparse state and enumeration from the
   same opened directory; publish index, directory ids and root-ancestor
   records as one snapshot; missing or legacy identity data denies.
3. **Identity contract.** Volume identity, full 64/128-bit ids, id reuse,
   remount/reformat/offline handling; handle-bound component resolution; scope
   out network and removable volumes.
4. **Impersonation per OS thread.** Every thread that opens directories
   installs the request token itself; no scheduler work inside the guard; a
   failed revert ends that execution domain.
5. **Server authentication.** Keep the connected-pipe owner check; one
   retained process handle; trusted install path; covers every write and
   reconnect. State that a compromised SYSTEM service can keep and reuse the
   token (trusted code).
6. **Token ownership and privileges.** Stream clones must not keep tokens;
   backup privilege is not exclusive to elevated admins (Backup Operators);
   prefer `SECURITY_EFFECTIVE_ONLY`.
7. **Blocking opens.** Deadlines do not bound a stalled open; bound outstanding
   I/O and tokens independently of the response; budgets cover the whole
   request.
8. (P2) **Connection exhaustion.** Per-SID connection and queue limits; idle
   clients cannot starve others.
9. **Adaptive disclosure.** Chosen prefix queries plus `partial`/`at_least`
   reveal hidden names' existence, not only population; prevent or get explicit
   acceptance.
10. **Test matrix** for all of the above (MIC no-read-up, not just a High label;
    no plain equality with Get-ChildItem for the stricter policy; real token
    capture attempts by a squatter).

Review-1 items still open: 1 (partial), 2, 7, 8, 11, 12, 13 (partial).
Scoped out as intended: 5, 6 (Unix). Resolved as requirements: 9, 10.
