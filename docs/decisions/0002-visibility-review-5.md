# ADR 0002 security review 5 (2026-11-07)

Reviewer: independent model review (Codex, GPT-6.1 Sol, high effort) of
revision 5 at `f707f3d`. Verdict: **accept with required changes**; gaps G1
and G2 stay accepted. Review-4 items 1–4 resolved.

1. (P1) **Exact path equality does not bind the current ancestors.** With
   delete sharing on the chain, another process can, between B's chain
   opens and the child open, move a listable directory `D` out, rename a
   listable `A₁` away, put a B-denied `A₂` at the same path, move `D`
   beneath it and move a secret file into `D`. The cached `D` handle still
   permits the relative child open and the child's current name equals the
   checked path, so rule 3 passes although `A₂` was never authorized. This
   is direct disclosure outside G1 and G2. Fix: keep the checked chain
   attached through admission, e.g. open every chain directory without
   `FILE_SHARE_DELETE` and hold the handles through the attachment check;
   deny on sharing conflicts or resource exhaustion. Add a deterministic
   mocked schedule and a disposable Windows CI probe.

Suggestion: broaden the user-facing G1 warning beyond "several accounts"
and "file names" to cover same-account sandboxed callers and metadata
inference.

Owner decision (2026-11-07): lock folders briefly while checking. Applied in
revision 6 (rule 4, held chain with a 5 ms hold limit), which also shrinks
G2 from 50 ms to the hold.
