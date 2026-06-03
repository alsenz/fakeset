# VAR-LINKED-CONTENT — Variants on linked content lists

## Status

**Future — stub.** Not designed. Deferred out of [`VAR-EXPAND`](done/VAR-EXPAND.md)
(open question Q4). Until this lands, variants on linked-content-list item fields
are **rejected at validation** (see VAR-EXPAND §Validation).

## What this is

A **linked content list** is a `links:` list field whose items are drawn, per
outer row, from a linked dataset (the staging / witness / assembly pipeline). This
spec would allow the *content fields* of such a list — the per-item fields — to be
**tagged unions** and have those unions lowered the same way VAR-EXPAND lowers
unions elsewhere: into cases + a discriminant, factored by the Bernoulli machinery.

("Linked content list" ≠ "linked list" — it names a list whose contents are linked
to another dataset.)

## Why it is deferred (not just hard)

Lowering a union sitting on a **witness-source** member interacts with the most
intricate part of the executor, and none of it was needed to ship VAR-EXPAND:

- The `n_eligible_slots` boundary in `GenerateWitness` (which leading rows of the
  combined batch are eligible linked-dataset slots) would have to account for the
  union's lowered cases.
- The `_staging_refs` dedup (the many-to-one pairing from source slots to drawn
  linked rows) and `_linked_idx` bookkeeping would need to be variant-aware.
- Output/identity of a witness row per `(case)` draw is unspecified.

These are real design questions with no obvious answer yet, and linked-content-list
unions appear to be rare in practice — so VAR-EXPAND bans them rather than guessing.

## Current behaviour (the gate this spec eventually lifts)

`lib/validate.rs` errors when a `type: variant` field appears among the content
fields of a linked content list:

> variants on linked content lists are not yet supported (see VAR-LINKED-CONTENT)

## To be designed

- Where the discriminant lives for a witness-row case, and how it survives the
  staging → witness → assembly fold.
- Interaction with `n_eligible_slots`, `_staging_refs`, `_linked_idx`.
- Whether per-item union choice is per drawn linked row or per source slot.
- Test plan (statistical + integration) once the design exists.

## Dependencies

| Spec | Reason |
|------|--------|
| VAR-EXPAND | Provides variant lowering (tagged union → cases + discriminant); this spec extends it onto witness-source members |
| (list-link pipeline) | The staging / witness / assembly machinery this must thread through |
