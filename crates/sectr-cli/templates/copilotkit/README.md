# __SECTR_APP_NAME__ — a Sectr agent with a CopilotKit chat

Scaffolded by `sectr init --frontend copilotkit`. Every Sectr route is an
[AG-UI](https://docs.ag-ui.com) endpoint, so CopilotKit's chat talks to it
directly. You get streaming, tool calls, and approvals as native interrupts,
and conversations are durable sessions: Sectr stores them, so your server
stores nothing.

- `agent/`: a support agent with a tool that needs human approval, on the
  `/chat` route (see `agent/README.md`).
- Everything else: a Next.js app with a CopilotKit chat.

## Run it locally (no keys needed)

```sh
cd agent && sectr dev
```

`sectr dev` prints the route's AG-UI URL:

```
- 🔌 connect via AG-UI at: https://local.sectr.dev:8420/ag-ui/__SECTR_APP_NAME__/chat
```

`.env.development` already points at it. In a second terminal, from this
directory:

```sh
npm run dev
```

If `sectr dev` says it picked another port (8420 was busy), put the URL it
printed in `.env.local`: `SECTR_AGUI_URL=<that URL>`.

Open http://localhost:3000 and try:

1. **Chat:** "Where is order 123?"
2. **Approve:** "Please refund it." The agent pauses on the `refund` tool.
   Approve or deny it in the chat.
3. **Reload:** the conversation is named in the URL (`?thread=…`) and lives
   on Sectr, so a reload picks it back up, approvals included.

Without an `OPENAI_API_KEY` the agent answers from a scripted mock
(`agent/fake.py`). To use a real model, set it in `agent/.env` (see
`agent/.env.example`).

## Deploy

```sh
cd agent
sectr deploy                      # prints the deployed AG-UI URL
sectr secrets set OPENAI_API_KEY  # optional: use a real model in the cloud
```

Then deploy the Next.js app anywhere Node runs. Set these for both the build
and the server:

| Variable | Value |
| --- | --- |
| `SECTR_AGUI_URL` | the URL `sectr deploy` printed |
| `SECTR_INVOCATION_KEY` | your app's invocation key (dashboard → Settings → API keys). Server-side only. |

The app's server is stateless: the Copilot Runtime runs with `SectrRunner`,
which keeps no copy of any conversation. A reloaded page, a second device, a
restarted or serverless instance all get the conversation from Sectr. Host it
however you like (Vercel, a container, `next start`).

## How it works

| File | What it does |
| --- | --- |
| `agent/app.py` | The agent and its `/chat` route |
| `app/api/copilotkit/[[...slug]]/route.ts` | The Copilot Runtime, with a `SectrAgent` for the route and `SectrRunner` |
| `app/api/agent-token/route.ts` | Mints a Sectr user token (`sks_…`) for the current user. This is the only place the invocation key is used. |
| `app/providers.tsx` | Fetches the token once per page load and passes it to CopilotKit. The Runtime forwards it to Sectr. |
| `app/chat.tsx` | `CopilotSidebar`, plus `useInterrupt` for the approve/deny UI |
| `proxy.ts` | Stand-in for your auth: an anonymous user id cookie, and a `?thread=` id per conversation |

`SectrAgent` and `SectrRunner` come from the [`@sectr/copilotkit`](https://www.npmjs.com/package/@sectr/copilotkit)
package. `SectrRunner` replaces CopilotKit's default in-memory runner: runs and
reconnects go straight to Sectr, so the server holds no conversation state.
`SectrAgent` is `@ag-ui/client`'s `HttpAgent` plus reconnecting a reloaded
page, which `SectrRunner` does through it.

`package.json` pins `@copilotkit/*` and `@ag-ui/client` to exact versions
that match: each CopilotKit release pins its own `@ag-ui/client`, and a second
copy of it doesn't type-check against CopilotKit's. Upgrade them together.

A user token can only see conversations owned by the user it was minted for,
and because the Runtime keeps nothing, there's no copy anywhere else for
another user to reach. In your app, replace the anonymous cookie with your
signed-in user's id. Sectr never authenticates your users; it trusts your
backend's mint call.

Docs: [Connecting your frontend](https://docs.sectr.dev/getting-started/connecting-frontend/).
The same app, with an agent on two routes, is the
[Sectr + CopilotKit starter](https://github.com/sectr-dev/sectr-copilotkit-starter).
