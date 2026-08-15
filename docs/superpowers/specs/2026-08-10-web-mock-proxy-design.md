# Web Mock Proxy Isolation Design

## Problem

`WebFetchTool` and `BochaSearchProvider` construct reqwest clients that inherit
environment proxy variables. In an environment with a proxy but no `no_proxy`
entry for loopback addresses, requests to a `wiremock` server on `127.0.0.1`
are sent through the proxy and fail.

## Design

Production constructors retain their existing default reqwest clients and keep
supporting user-configured proxies. Test-only constructors accept or create a
reqwest client built with `.no_proxy()`. The web mock tests use that test path,
so they always connect directly to their local mock server without mutating
process-wide environment variables.

## Validation

Run `cargo test -p yi-agent-tools --lib` while the current proxy environment is
present. All WebFetch and Bocha mock tests must pass; production client
construction remains unchanged.
