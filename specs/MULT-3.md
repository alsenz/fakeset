# MULT-3: Include convenience helpers

Minor / cosmetic field helpers to make working with includes easier.

_Note_: The fundamental architectural tenet that children by inclusion are generated first, and data subsequently accumulates towards parents in segments, never changes.

## Features

### 1. `include.fields` / `include.couple.fields` — field wildcard copy

A string list of fields (supporting `*` wildcards) that are automatically and implicitly copied into the `fields` definition of the associated child dataset or nested include list. Eliminates repetitive field declarations when the child inherits many fields from its include.

```yaml
include:
  file: people.yaml
  ref: person
  fields: ["*"]          # copy all fields from people.yaml
  exclude: [internal_id] # optional: fields to suppress
```

`exclude` is only valid alongside `fields`.

Applies to both `include` (top-level) and `include.couple` (coupled pool). For nested list includes, `content.include.fields` copies fields into the list's `content.fields`.

### 2. `content.include.project_field` — single-field projection

Only valid when `content.include` is present. Names one field from the included dataset to project, so that the nested list becomes a simple-type list (string, number, etc.) rather than always an object list.

```yaml
- name: on_call_doctors
  type: list
  content:
    include:
      file: doctors.yaml
      ref: doctors
      cardinality: {min: 2, max: 5}
      project_field: full_name    # project to list of strings
```

Output: `on_call_doctors: ["Dr Alice", "Dr Bob", ...]` rather than a list of objects.

When `project_field` is used, no `content.fields` are needed (and it is a validation error to combine them).

### 3. `fields[].hidden` — suppress field from output

A boolean that causes a field to be computed as part of execution but excluded from the final output. Useful to express `refs`/`bind` collect bindings without polluting the output with redundant fields.

```yaml
- name: allocated_to
  type: string
  hidden: true
  refs:
    - ward_name
    - {bind: doctors.on_call_list, reducer: collect}
```

`hidden: true` fields participate fully in validation, ref resolution, and execution. They are stripped from emitted output by `filter_hidden_columns` in the same pass that strips sentinel columns.