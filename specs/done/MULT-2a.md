# MULT-2a: Structural redesign — unified `Include` type

**Prerequisite for MULT-2.** Collapses `Include`, `ContentInclude`, and `Couple` into one unified `Include` type, moves pool-partner declarations out of the `include:` block and into a dataset-level `links:` list, and replaces `content.include:` on list fields with `content.group: <ref>`. This is a pure model-and-YAML refactor — no execution capabilities change, no generated output changes. All MULT-1 tests pass after fixture YAMLs are updated.

_Note_: The fundamental architectural tenet that children by inclusion are generated first, and data subsequently accumulates towards parents in segments, never changes.

**Prerequisites:** MULT-1 complete.

---

## Changes

### S1. `couple` → dataset-level `links`

**Before (MULT-1 model):**
```yaml
include:
  file: individuals.yaml
  ref: individuals
  ratio: 0.1
  cardinality: {min: 1, max: 6}
  couple:                        # nested one level inside include
    file: organisations.yaml
    ref: organisations
    ratio: 1.0
```

**After:**
```yaml
include:
  file: individuals.yaml
  ref: individuals
  ratio: 0.1
  cardinality: {min: 1, max: 6}
links:                           # dataset-level sibling of include; a list
  - file: organisations.yaml
    ref: organisations
    ratio: 1.0
```

`SyntheticDataset` gains `links: Vec<Include>` (default empty). The `Couple` struct is deleted; the same `Include` struct is reused for each link entry. Each link entry supports all `Include` fields (`file`, `ref`, `ratio`, `cardinality`, `reinforcement`), with context-specific validation applied at use sites.

A link with no `content.group:` reference in the dataset's fields is a **junction link** (directorships style). Junction links remain a validation error in MULT-2a and are activated in MULT-2.

### S2. `content.include` → dataset-level `links` + `content.group`

**Before (MULT-1 model):**
```yaml
name: wards
rows: 8
data:
  - name: on_call_doctors
    type: list
    content:
      include:
        file: doctors.yaml
        ref: doctors
        cardinality: {min: 2, max: 5}
        ratio: 0.33
      fields:
        - name: doctor
          type: string
          ref: doctors.full_name
```

**After:**
```yaml
name: wards
rows: 8
links:
  - file: doctors.yaml
    ref: doctors
    cardinality: {min: 2, max: 5}
    ratio: 0.33
data:
  - name: on_call_doctors
    type: list
    content:
      group: doctors
      fields:
        - name: doctor
          type: string
          ref: doctors.full_name
```

`group: <ref>` on `content:` names the link (by its `ref:` value) whose rows are grouped by outer slot to produce this list's items. The include spec moves to `links:`. A link referenced by a `group:` field is a **list link**; its execution pipeline (GenerateInnerFlat / AssembleNestedInclude) is unchanged.

**Validation:**
- `group: <ref>` must resolve to an entry in `links`. Error if no match.
- A link may be referenced by at most one `group:` field. Error if two fields share the same group ref.

### S3. `Include` is the only include type

After S1 and S2, `Couple` and `ContentInclude` are deleted. `Include` covers all three former roles: driver (`dataset.include`), list links, junction links. Validation dispatches on context (has-group vs. no-group). No further YAML changes beyond S1 and S2.

---

## Implementation plan

### Stage 1 — Model changes (`../../lib/models.rs`)

**Delete `Couple` struct.**

**Remove `couple: Option<Couple>` from `Include`.**

**Delete `ContentInclude` struct.**

**Remove `include: Option<ContentInclude>` from `ListContent`; add `group: Option<String>`:**
```rust
pub struct ListContent {
    pub group: Option<String>,    // replaces: include: Option<ContentInclude>
    #[serde(flatten)]
    pub item: Field,
}
```
The `#[serde(flatten)]` on `item` means YAML `fields:` at the content level continues to map to `item.fields` — no YAML-structure change for the item spec.

**Add `links` to `SyntheticDataset`:**
```rust
#[serde(default)]
pub links: Vec<Include>,
```

**Update `Field::is_nested_include()`:**
```rust
pub fn is_nested_include(&self) -> bool {
    self.content.as_deref().is_some_and(|c| c.group.is_some())
}
```

**Replace `for_each_content_include` with `for_each_nested_include`:**

The visitor now receives the matching `&Include` from `dataset.links` rather than a `&ContentInclude` from `content.include`. Field names on `Include` and `ContentInclude` are identical (`file`, `reference`, `ratio`, `cardinality`, `reinforcement`), so all visitor bodies compile unchanged after the type-level rename.

```rust
/// Visits all fields (recursively) with `content.group` set, paired with the
/// matching link from `links` and the field's `content.item.fields`.
pub fn for_each_nested_include<'a>(
    links: &'a [Include],
    fields: &'a [Field],
    visitor: &mut impl FnMut(&'a Field, &'a Include, &'a [Field]),
) {
    for field in fields {
        if let Some(content) = &field.content {
            if let Some(group_ref) = &content.group {
                if let Some(link) = links.iter().find(|l| &l.reference == group_ref) {
                    visitor(field, link, &content.item.fields);
                }
            }
            for_each_nested_include(links, &content.item.fields, visitor);
        }
        for_each_nested_include(links, &field.fields, visitor);
    }
}
```

Delete `for_each_content_include`. All callers are updated in Stage 2.

**Verification:** `cargo check` — errors only in downstream callers; `../../lib/models.rs` itself must compile cleanly.

---

### Stage 2 — Ripple changes (all other modules)

Mechanical. Fix each module in sequence until `cargo check` is clean.

**`../../lib/graph.rs`:**

Replace `for_each_content_include` → `for_each_nested_include(&dataset.links, &dataset.data, ...)`. The visitor type changes from `&ContentInclude` to `&Include`; field accesses (`inc.file`, etc.) are identical.

For each link in `dataset.links`, add a DAG edge from the resolved link path to the current dataset regardless of whether the link is a list link or a junction link. This is consistent with MULT-1's approach: structural edges appear in the DAG; validation prevents junction links from reaching execution.

**`../../lib/validate.rs`:**

- Replace `c.include.is_none()` → `c.group.is_none()` for the "plain list" validation branch.
- Replace `for_each_content_include` → `for_each_nested_include(&dataset.links, &dataset.data, ...)`.
- Remove the MULT-1 validation error for `include.couple` (field no longer exists on `Include`).
- Add validation rules for `links`:
  - Each link's `file` must resolve (same `resolve_include` check used for the driver `include`).
  - A link not referenced by any `content.group:` in the dataset's fields → error: `"link '{ref}' in '{name}': junction links are not yet supported; activate in MULT-2"`.
  - `group: <ref>` where `<ref>` does not match any entry in `links` → error.
  - Two `content.group:` fields referencing the same link ref → error.
- `validate_rich_content` signature: change `rich_include: &ContentInclude` → `link: &Include`. Body unchanged.

**`../../lib/rewrite.rs`:**

- Replace `content.include.is_some()` → `content.group.is_some()` in the nested-include branch.
- In `resolve_refs`: look up the matching link from `dataset.links` by `content.group` ref. Pass the `&Include` to `resolve_nested_include_content_field`.
- `resolve_nested_include_content_field` parameter: `content_include: &ContentInclude` → `link: &Include`. Body unchanged.
- `resolve_field` and `resolve_to_base`: no changes (use `dataset.include: Option<Include>`, unaffected by the S1/S2 changes).

**`../../lib/plan.rs`:**

- Replace `for_each_content_include` → `for_each_nested_include(&dataset.links, &dataset.data, ...)`.
- In `emit_nested_include_steps`: look up the link by matching `content.group` ref against `dataset.links`. Pass the link to the `GenerateInnerFlat` step.
- `GenerateInnerFlat` step variant: change `include: ContentInclude` → `include: Include`.
- `collect_pool_siblings`: replace `for_each_content_include` → `for_each_nested_include`. Visitor body accesses the same fields (`.ratio`, `.reference`) — no body changes.
- Remove any code reading `dataset.include.as_ref().and_then(|i| i.couple...)`. Junction link planning is a validation error in MULT-2a; no stub needed.

**`../../lib/executor.rs`:**

- `execute_inner_flat` signature: `include: &ContentInclude` → `include: &Include`. Body unchanged.
- `GenerateInnerFlat` match arm in `execute`: destructure `include: Include` instead of `include: ContentInclude`.
- No couple-execution code to remove (MULT-1 kept it as a validation error; nothing was wired in the executor).

**`../../lib/expressions.rs`, `../../lib/schema.rs`:**

No `Couple` / `ContentInclude` references expected. No changes.

**Verification after Stage 2:** `cargo check` — zero errors. `cargo test` — plan/validate/graph tests pass; executor tests using `rich_list`, `bernoulli_rich_list`, `count_normal`, `rich_list_plain` fixtures will fail (their YAMLs still use `content.include:`, now silently dropped by serde). Fixed in Stage 3.

---

### Stage 3 — Update fixture YAMLs

**Executor fixtures** — four files use `content.include:` and need updating to `links:` + `content.group:`:

- `tests/fixtures/execute/rich_list/events.yaml`
- `tests/fixtures/execute/bernoulli_rich_list/events.yaml`
- `../../tests/fixtures/execute/count_normal/outer.yaml`
- `tests/fixtures/execute/rich_list_plain/records.yaml`

Pattern for each (using `rich_list/events.yaml` as the example):

```yaml
# Before:
name: events
rows: 5
data:
  - name: attendees
    type: list
    content:
      include:
        file: people.yaml
        ref: person
        ratio: 0.5
        cardinality: {min: 1, max: 4}
      fields:
        - name: name
          ref: person.full_name

# After:
name: events
rows: 5
links:
  - file: people.yaml
    ref: person
    ratio: 0.5
    cardinality: {min: 1, max: 4}
data:
  - name: attendees
    type: list
    content:
      group: person
      fields:
        - name: name
          ref: person.full_name
```

**Validation fixture** — `../../tests/fixtures/validation/include_couple/child.yaml` uses `couple:` inside `include:`. After MULT-2a the YAML changes to use `links:` at dataset level, and the expected error changes from the MULT-1 "couple not yet supported" message to the MULT-2a "junction links are not yet supported" message. Update both the fixture YAML and the expected error string in the test.

**Straggler check:**
```bash
grep -rn 'ContentInclude\|for_each_content_include\|content\.include\|\.couple\b\|Couple\b' lib/ tests/
# → zero results expected
```

**Verification:** `cargo test` — all tests pass. Zero regressions vs. MULT-1 post-state.

---

### Stage 4 — Final verification

```bash
cargo check      # zero errors, zero new warnings
cargo test       # all tests pass (identical set to post-MULT-1)
```

Remove unused imports for deleted types. Fix any dead-code warnings introduced by the deletions.
