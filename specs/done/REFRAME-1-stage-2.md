# REFRAME-1 Stage 2 — Naming pass

Pure renames: no algorithmic or structural changes. Every symbol, constant, CLI flag,
doc comment, and printed string uses the new vocabulary after this stage.
Verify with `cargo check` + `cargo test` at the end — no behavioural change expected.

---

## `lib/segment.rs`

| Old | New |
|-----|-----|
| `pub struct Sibling` | `pub struct LowerCoverMember` |
| `pub is_pool: bool` | `pub is_witness_source: bool` |
| `pub const DEFAULT_MAX_SIBLINGS: usize` | `pub const DEFAULT_MAX_LOWER_COVER: usize` |
| `Segment.siblings: Vec<PathBuf>` | `Segment.members: Vec<PathBuf>` |
| `fn sibling_field_constraints` | `fn lower_cover_field_constraints` |
| `fn plan_segments(..., siblings: &[Sibling], max_siblings: usize)` | `plan_segments(..., members: &[LowerCoverMember], max_lower_cover: usize)` |
| `fn precompute_conflicts(siblings: &[Sibling])` | `fn precompute_conflicts(members: &[LowerCoverMember])` |
| local `sib` / `sibs` / `n_siblings` | `member` / `members` / `n_members` |

Internal test helper: `is_pool: false` → `is_witness_source: false`;
`plan_segments(..., DEFAULT_MAX_SIBLINGS)` → `plan_segments(..., DEFAULT_MAX_LOWER_COVER)`.

All doc comments: "sibling group" → "parent + lower cover", "siblings" → "lower cover members".

---

## `lib/models.rs`

| Old | New |
|-----|-----|
| `ListContent.group: Option<String>` | `ListContent.from: Option<String>` with `#[serde(alias = "group")]` |
| `fn is_link_content(&self) -> bool` | `fn is_list_link(&self) -> bool` |
| `fn for_each_link_content<'a>(...)` | `fn for_each_list_link<'a>(...)` |

Inside `for_each_list_link`: `content.group` → `content.from`.

Doc comment on `ListContent`: "nested include" → "list-link field"; "pool dataset" →
"linked dataset"; "pool-scoped ref" → "linked-dataset ref".

Doc comment on `SyntheticDataset.links`: "Pool/partner datasets" → "Linked datasets";
"pool-scoped values" → "linked-dataset values"; "nested-include pipeline" →
"witness/assembly pipeline".

---

## `lib/plan.rs`

**`ExecutionStep` variant renames:**

| Old variant | New variant | Field renames |
|-------------|-------------|---------------|
| `GenerateInnerFlat { flat_key, outer_path, ..., pool_slots_path }` | `GenerateWitness { witness_key, staging_path, ..., linked_path }` | `flat_key` → `witness_key`; `outer_path` → `staging_path`; `pool_slots_path` → `linked_path` |
| `AssembleNestedInclude { outer_path, dataset, flat_specs }` | `AssembleFromWitness { staging_path, dataset, witness_specs }` | `outer_path` → `staging_path`; `flat_specs` → `witness_specs` |
| `CollectToPool { pool_path, pool_field, group_by: "_pool_idx", ... }` | `AccumulateToLinked { linked_path, linked_field, group_by: "_linked_idx", ... }` | `pool_path` → `linked_path`; `pool_field` → `linked_field`; hardcoded string `"_pool_idx"` → `"_linked_idx"` |
| `GenerateSiblingGroup { ..., siblings, skip_parent_emit }` | `GenerateLowerCoverGroup { ..., members, skip_parent_emit }` | `siblings` → `members`; `skip_parent_emit` unchanged until Stage 3 |

**`PrefillSource` struct** → `InheritedField` (fields `from_path`, `from_column`, `into_column` unchanged).

**Function renames:**

| Old | New |
|-----|-----|
| `fn build_sibling_groups` | `fn build_lower_cover_groups` |
| `fn collect_pool_siblings` | `fn collect_linked_lower_cover_members` |
| `fn pool_sibling_path` | `fn linked_lower_cover_path` |
| `fn inner_flat_key` | `fn witness_key` |
| `fn emit_nested_include_steps` | `fn emit_witness_steps` |
| `fn push_with_nested_include` | `fn push_with_list_link_steps` |
| `fn check_case2_collect_restrictions` | `fn check_collect_segmentation_restrictions` |

**Inside `emit_witness_steps`** (was `emit_nested_include_steps`):
- `content.group` → `content.from` (accessing `ListContent.from` after the models rename)
- `is_link_content()` → `is_list_link()` (call site in `push_with_list_link_steps`)
- local `flat_key` → `witness_key`; `pool_slots_path` → `linked_path`
- comment "CollectToPool" → "AccumulateToLinked"

**Local variables** throughout: `pool_path` → `linked_path`; `pool_sibling` → `linked_member`;
`is_pool` → `is_witness_source`; `sibs` / `sib` / `n_siblings` → `members` / `member` / `n_members`.

Doc comment on `GenerateWitness` (was `GenerateInnerFlat`): remove "inner flat", "pool slot",
"pool-scoped refs" — replace with witness/staging/linked vocabulary.

**Imports**: `PrefillSource` → `InheritedField`; `Sibling` → `LowerCoverMember`;
`DEFAULT_MAX_SIBLINGS` → `DEFAULT_MAX_LOWER_COVER`.

---

## `lib/executor.rs`

**Function renames:**

| Old | New |
|-----|-----|
| `fn execute_inner_flat` | `fn execute_witness` |
| `fn execute_assemble_nested_include` | `fn execute_assemble_from_witness` |
| `fn execute_collect_to_pool` | `fn execute_accumulate_to_linked` |
| `fn inject_pool_idx` | `fn inject_linked_idx` |
| `fn strip_pool_idx` | `fn strip_linked_idx` |

**Column name** `"_pool_idx"` → `"_linked_idx"` everywhere: in `prepend_column` calls, SQL
strings (if any), doc comments, and the `strip_sentinel` calls.

**Match arm** `ExecutionStep::GenerateSiblingGroup` → `ExecutionStep::GenerateLowerCoverGroup`;
destructure `siblings` → `members`. Dispatch to `execute_sibling_group` (function rename:
`execute_sibling_group` → `execute_lower_cover_group`; parameter `siblings: &[Sibling]` →
`members: &[LowerCoverMember]`).

**Match arm** `ExecutionStep::CollectToPool` → `ExecutionStep::AccumulateToLinked`; destructure
`pool_path` → `linked_path`, `pool_field` → `linked_field`. Dispatch to `execute_accumulate_to_linked`.

**Inside `execute_witness`** (was `execute_inner_flat`):
- local `pool_slots` → `linked_batch`
- doc comments: "pool slot", "pool-scoped ref", "pool sampling" → linked-dataset vocabulary

**Imports**: `PrefillSource` → `InheritedField`; `Sibling` → `LowerCoverMember`.

---

## `lib/graph.rs`, `lib/validate.rs`, `lib/rewrite.rs`, `lib/expressions.rs`

**`graph.rs`**: No symbol renames currently needed (grep shows no old-vocab symbols). Update
any doc comments that use "pool dataset", "pool sibling", "nested-include", or "rich list"
vocabulary.

**`validate.rs`**: String literals to update:
- `"count cannot be set on a nested-include list field"` → `"count cannot be set on a list-link field"`
- `"nested include content at …"` → `"list-link content at …"`
- `"expression is not supported inside nested include content"` → `"expression is not supported inside list-link content"`
- `"pool dataset not loaded"` → `"linked dataset not loaded"`
- Comment `// Case 2 — fields inside nested-include content blocks` → `// Case 2 — fields inside list-link content blocks`

**`rewrite.rs`**: Symbol and string updates:
- `fn resolve_nested_include_content_field` → `fn resolve_list_link_content_field`
- Error strings: `"nested include field '{}': ..."` → `"list-link content field '{}': ..."`
- Comment `// Resolve pool-scoped refs inside nested include content` → new vocabulary

**`expressions.rs`**: No symbol renames expected (grep shows no old-vocab identifiers). Update
any comments that use "nested include" or "pool" vocabulary.

---

## `src/main.rs`

**CLI flag**: `--max-siblings` → `--max-lower-cover`.
Help string: `"Maximum number of lower cover elements per group. Enumeration cost is 2^N; raising this costs RAM quadratically. Default: 16."`

**Import**: `segment::DEFAULT_MAX_SIBLINGS` → `segment::DEFAULT_MAX_LOWER_COVER`.

**`print_plan` string replacements** (exact strings from current source):

| Old string | New string |
|------------|------------|
| `"inner flat:"` (in the `GenerateInnerFlat` arm label) | `"witness:"` |
| `"assemble nested include:"` | `"assemble from witness:"` |
| `"collect to pool:"` | `"accumulate to linked:"` |
| `"sibling group:"` | `"lower cover group:"` |
| `"siblings:"` (the sub-list label) | `"lower cover:"` |
| `"(parent-only)"` (segment label) | `"(remainder)"` |
| `"[nested include content]"` | `"[list-link content]"` |
| `"prefill:"` | `"inherits:"` |

Match arm renames: `GenerateSiblingGroup` → `GenerateLowerCoverGroup`;
`GenerateInnerFlat` → `GenerateWitness`; `AssembleNestedInclude` → `AssembleFromWitness`;
`CollectToPool` → `AccumulateToLinked`. Field destructuring: `flat_key` → `witness_key`,
`flat_specs` → `witness_specs`, `pool_path` → `linked_path`, `pool_field` → `linked_field`,
`siblings` → `members`.

---

## `tests/executor_tests.rs`

**Test function renames** (no logic changes, only `fn` names and comments):

| Old | New |
|-----|-----|
| `test_inner_flat_slot_idx` | `test_witness_slot_idx` |
| `test_bernoulli_nested_include_parent_assembles_correctly` | `test_bernoulli_list_link_parent_assembles_correctly` |
| `test_plain_fields_in_nested_include_content` | `test_plain_fields_in_list_link_content` |
| `test_nested_include_refs` | `test_list_link_refs` |
| `test_nested_include_collect_to_pool` | `test_list_link_collect_to_linked` |
| `test_variant_sibling_total_rows` | `test_variant_lower_cover_total_rows` |

**Assertion string / field access renames**:
- `"_pool_idx must not appear in wards output"` → `"_linked_idx must not appear in wards output"`
- `"_pool_idx must not appear in directorships output"` → `"_linked_idx must not appear in directorships output"`
- `ward.get("_pool_idx")` → `ward.get("_linked_idx")`
- `row.get("_pool_idx")` → `row.get("_linked_idx")`

**Section header comments**: update "sibling", "pool", "nested include", "inner flat" to new
vocabulary (e.g. `// _slot_idx and _pool_idx sentinel tests` → `// _slot_idx and _linked_idx sentinel tests`).

**Inline comments** (illustrative; update any others found during the pass):
- `"Each ward has an on_call_doctors list drawn from doctors via _pool_idx"` →
  `"drawn from doctors via witness batch"`
- `"pool val should be in [1, 10]"` → no change needed (this refers to a YAML field value, not a sentinel)

---

## Fixture YAML files (`group:` → `from:`)

23 files require a single-field rename. In each file: `group: <ref>` → `from: <ref>`.
Once all fixtures are migrated, remove the `#[serde(alias = "group")]` from `ListContent.from`.

Files (relative to repo root):

```
tests/fixtures/execute/include_fields_list_link/events.yaml
tests/fixtures/execute/bernoulli_link_content/events.yaml
tests/fixtures/execute/hidden_collect_binding/outer.yaml
tests/fixtures/execute/link_content_plain/records.yaml
tests/fixtures/execute/no_replacement/outer.yaml
tests/fixtures/execute/project_list/events.yaml
tests/fixtures/execute/wards_doctors/wards.yaml
tests/fixtures/execute/link_content/events.yaml
tests/fixtures/execute/count_normal/outer.yaml
tests/fixtures/validation/link_content_expression_in_content/main.yaml
tests/fixtures/validation/link_content_include_scoped_with_type/main.yaml
tests/fixtures/validation/project_ref_mismatch/outer.yaml
tests/fixtures/validation/link_content_outer_scoped_missing_field/main.yaml
tests/fixtures/validation/project_field_missing/outer.yaml
tests/fixtures/validation/link_content_outer_scoped_no_type/main.yaml
tests/fixtures/validation/count_on_nested_include_list/main.yaml
tests/fixtures/validation/collect_bind_not_list/outer.yaml
tests/fixtures/plan/nested_collect/outer.yaml
tests/fixtures/plan/case2_collect_joint_segment/outer.yaml
tests/fixtures/validation/project_with_fields/outer.yaml
tests/fixtures/validation/link_content_include_scoped_missing_field/main.yaml
tests/fixtures/plan/reinforcement_zero_infeasible/outer.yaml
tests/fixtures/validation/collect_bind_no_default/outer.yaml
```

After migrating all 23 files, remove the serde alias from `ListContent.from` (one-line edit
to `models.rs`).

**Deliverable**: `cargo check` passes; all 173+ tests pass; every human-readable symbol and
string uses new vocabulary. No behavior change.
