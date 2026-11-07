# ADR 0002 security review 4 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
revision 4 at `69ff18a`. Verdict: **accept with required changes**; gap G1
stays accepted.

1. (P1) **Retained grants and revocation.** Windows checks access when a
   handle is opened and never revokes it; a request reusing a directory handle
   can admit a name moved into that folder after B lost access. Establish
   freshness through row admission, or state retained-handle semantics and get
   owner acceptance. Add a deterministic revoke-then-insert probe.
2. (P1) **Per-directory case sensitivity.** A volume-wide case-insensitive
   comparison can accept a path through a denied `a` when B may list `A`.
   Compare exactly (hide case-only changes) or use each directory's real case
   semantics; distinct names in a case-sensitive folder are not aliases.
3. (P2) **Count** must apply the fresh whole-expression rule like search.
4. (P2) **G1 wording** must cover any admitted caller whose token cannot list
   indexed content (including restricted processes of the same account) and
   every searchable indexed property.

Review-3 items: 3 and 5 resolved; 1 resolved for returned rows (Count: item 3
above); 2 open (items 1 and 2 above); 4 accepted as G1.
Notes: a rename after the attachment check is within the per-row policy; a
hard link reporting another link's path must be hidden, never substituted;
`FileNameInfo` must be a complete volume-relative name starting with `\`.

Revision 5 response: exact UTF-16 comparison (item 2); Count applies all rules
(item 3); G1 broadened (item 4); grants reusable for at most 50 ms, stated as
known gap G2, owner acceptance required (item 1).
