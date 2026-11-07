# ADR 0002 security review 1 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of commit
`24f9b33`; source and OS documentation only. Verdict: **accept with required
changes; not safe to implement unchanged; keep the ordinary-caller gate closed
until resolved.** The stricter-than-bypass-traverse rule is acceptable as
policy, but `FILE_LIST_DIRECTORY` alone is not stricter for callers without
bypass-traverse.

Required changes (P1 unless noted):

1. **Freshness.** Scan-time descriptors go stale when access is revoked; a
   restart preserves the exposure. Define a freshness point: security,
   owner, move, replacement and mount changes invalidate; watcher or journal
   gaps make the scope unavailable until revalidated. TTLs or eventual rescans
   do not satisfy the rule. Define in-flight requests during revocation.
2. **Atomic snapshot binding.** Publish one immutable object (index,
   visibility section, class table, unique security version); caches key on
   it; ranks and classes never cross snapshots; publish only complete masks.
3. **Token equivalence.** A hash of SIDs/integrity misses capabilities,
   restriction modes (`WRITE_RESTRICTED`), privileges and claims. Cache
   against the retained token object; share across tokens only with proven
   equivalence. Identification-level tokens suffice for `AccessCheck`.
4. **Complete Windows policy.** Require traverse where the OS does (or
   list+traverse throughout); evaluate mandatory labels
   (`LABEL_SECURITY_INFORMATION`); deny on conditional/callback ACEs or
   central policies; check both the call result and the access result; never
   apply the service's own privileges.
5. **Unix groups.** `getgrouplist` reads the account database, not the
   caller's effective groups. Use OS-authenticated peer groups or deny.
6. **Unix ACLs.** Detect ACLs natively per platform (macOS ACLs too); deny
   ACL-bearing directories until evaluated, with no blanket owner exception;
   MAC/sandbox (TCC, LSM) cannot be enforced by mode bits.
7. **Root ancestors.** A configured root's own ancestors must be checked too;
   define file roots; the bare-root exception covers only the root's name.
8. **Object binding and aliases.** Capture descriptors from the same opened
   directory object that is enumerated; no link traversal in the service
   until bound; define junctions, mount points, hard links, case and 8.3
   aliases; exclude alternate data streams.
9. **Mandatory filter.** Visibility must apply on every search and count
   path, outside the query expression, before limits and highlights; never
   reuse the optional `prefilter_bits` optimisation as the guard; corrupt or
   missing data never falls back to unfiltered.
10. **Status.** Define a versioned filtered status without totals; build it
    from an allowlist; never forward report strings (they carry counts and
    paths).
11. (P2) **Timing oracle.** Prefix ranges over global name order leak hidden
    populations through timing; define the boundary and avoid work
    proportional to hidden ranges.
12. (P2) **Per-principal work bounds.** Quotas, fair scheduling, bounded and
    coalesced builds, bounded descriptor tables; cancellation covers
    authorization work.
13. **Acceptance matrix.** Regression cases for all of the above are merge
    gates, run only on disposable CI systems.
