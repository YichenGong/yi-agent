# OpenAI Process Selector Schema Compatibility Design

## Goal

Allow OpenAI-compatible providers to accept the managed-process tool definitions
while preserving the existing requirement that callers select a process by exactly
one of `process_id` or `name`.

## Design

`process_read` and `process_kill` share `process_selector_schema`. The helper
will continue to return a top-level JSON object with the selector properties and
any read-specific properties, but it will no longer add top-level `oneOf`.

The `selector_from_args` runtime validator remains the sole enforcement point
for exactly-one selector semantics. This is already shared by both tools and
returns clear errors for zero or two selectors.

## Verification

Add a unit test that constructs both schemas and asserts that their top-level
objects contain `type: "object"` and no `oneOf`, `anyOf`, `allOf`, `enum`,
`const`, or `not`. Keep the existing call-path test to verify valid selector
arguments still execute successfully.

## Scope

This change is limited to the managed-process input schemas and their tests. It
does not enable OpenAI strict mode or alter function execution, permission
checks, or the provider request format.
