# Grep Output Limits Design

## Goal

Prevent broad `grep` calls from adding unbounded tool-result content to the
model conversation.

## Scope and Design

`GrepTool` applies one shared internal budget to `content`,
`files_with_matches`, and `count`. While it walks files, it counts each
rendered result record and its UTF-8 byte length. It stops before adding a
record that would exceed 200 entries or 32 KiB, then adds one truncation notice
that tells the model to narrow `path`, `glob`, or `pattern`, or use another
output mode.

The limit is internal instead of a schema option, so a model cannot bypass the
context safety guard. Calls below the limits retain their existing behavior.

## Validation

Unit tests cover excess matching file paths and large matching content. Both
assert a truncation marker; the existing grep test suite verifies normal modes.
