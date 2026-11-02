---
title: API overview
description: The public HTTP API — one event journal, SSE tails, and a handful of verbs.
---

<!-- This was last checked for drift at commit 185df8e09c033928fa23fb53105ea8e3d99d3f36. -->

Any client - a web frontend, a mobile app, a script - drives a deployed
agent over the Sectr HTTP API. See the [full reference](/reference/api-spec/) for complete details.

<!-- skill: spec lookup — how to search openapi.json, in the skill only -->

## The primary concept

<!-- skill: skip section — SKILL.md states this -->

The **journal is the only source of truth**. A session is an append-only
event log; every "live" view - including the chat stream - is a tail of that journal.
This makes reconnecting (`Last-Event-ID`) trivially correct and produces a fully auditable
and debuggable transcript.

## The data plane APIs

| Area | Endpoints | Auth |
| --- | --- | --- |
| Sessions | [`POST /sessions`](/reference/api-spec/#api-1/POST/sessions), [`GET /sessions/{sessionId}`](/reference/api-spec/#api-1/GET/sessions/{sessionId}), [`POST /sessions/{sessionId}/input`](/reference/api-spec/#api-1/POST/sessions/{sessionId}/input) | user token (end users), invocation key (backend), or platform key |
| Streaming | [`GET /sessions/{sessionId}/events`](/reference/api-spec/#api-1/GET/sessions/{sessionId}/events) (SSE) | same as the session's |
| Approvals | [`POST /sessions/{sessionId}/approval`](/reference/api-spec/#api-1/POST/sessions/{sessionId}/approval), [`GET /approvals`](/reference/api-spec/#api-1/GET/approvals) | decide: user token (own sessions) / keys; inbox: platform key |
| AG-UI | [`POST /ag-ui/{app}/{route}`](/reference/api-spec/#api-1/POST/ag-ui/{app}/{route}) (run), [`GET /ag-ui/{app}/{route}`](/reference/api-spec/#api-1/GET/ag-ui/{app}/{route}) (reconnect, `?threadId=`) | user token or invocation key |

Browser clients authenticate with a user token, which your backend mints
with its invocation key via [`POST /tokens`](/reference/api-spec/#api-1/POST/tokens);
see [authentication](/reference/api/auth/).

## The control plane APIs

| Area | Endpoints | Auth |
| --- | --- | --- |
| Projects | [`GET /apps`](/reference/api-spec/#api-1/GET/apps), [`GET /apps/{appId}`](/reference/api-spec/#api-1/GET/apps/{appId}), [`GET /apps/{appId}/deployments`](/reference/api-spec/#api-1/GET/apps/{appId}/deployments) | platform key |
| Deployments | [`POST /deployments`](/reference/api-spec/#api-1/POST/deployments), [`POST /deployments/{deploymentId}/activate`](/reference/api-spec/#api-1/POST/deployments/{deploymentId}/activate) | platform key |
| Secrets | [`PUT /secrets/{key}`](/reference/api-spec/#api-1/PUT/secrets/{key}), [`GET /secrets`](/reference/api-spec/#api-1/GET/secrets), [`DELETE /secrets/{key}`](/reference/api-spec/#api-1/DELETE/secrets/{key}) | platform key |
| API keys | [`GET /api-keys`](/reference/api-spec/#api-1/GET/api-keys), [`POST /api-keys`](/reference/api-spec/#api-1/POST/api-keys), [`DELETE /api-keys/{keyId}`](/reference/api-spec/#api-1/DELETE/api-keys/{keyId}) | platform key |

## API Base URLs

- Prod: `https://api.sectr.dev`
- Locally, `sectr dev` serves the same API from its gateway, printed at server startup (`https://local.sectr.dev:8420` by default)

## Start here

1. [Authentication](/reference/api/auth/) — which key class you need
2. [Send a message](/reference/api/send-message/) — create a session, add turns
3. [Stream a session](/reference/api/stream/) — the SSE tail + resumption
4. [Approve or deny from your UI](/reference/api/approvals/)
5. [AG-UI endpoint](/reference/api/ag-ui/) — CopilotKit and other AG-UI clients
6. [Limits](/reference/api/limits/) — concurrency, rate limits, and the `429` codes
