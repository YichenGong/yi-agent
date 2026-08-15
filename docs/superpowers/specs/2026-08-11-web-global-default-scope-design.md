# Web Global Default Scope Design

## Goal

Make the web configuration scope selector reflect the common workflow: show
global configuration first and select it by default when it is available.

## Behavior

- Render the scope controls in `Global | Local` order.
- Initialize the selected scope to `global`.
- After configuration data loads, retain `global` when `globalEnvPath` exists.
- When no global configuration path is available, hide the global control and
  select `local` so the editor always has a valid writable scope.
- Clicking either available scope continues to update the active scope for the
  current page session; no browser persistence is introduced.

## Implementation Boundary

The change is confined to the static web asset in `yi-agent-web`. The API and
save request format already support both scopes and do not need to change.

## Verification

Add or update an asset-level test that asserts the control order and initial
scope, then run the relevant `yi-agent-web` test suite.
