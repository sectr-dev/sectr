# @sectr/copilotkit

Run [CopilotKit](https://docs.copilotkit.ai) on [Sectr](https://sectr.dev).
Every Sectr route is an [AG-UI](https://docs.ag-ui.com) endpoint, so
CopilotKit's chat components talk to your agent as is. This package adds the
two pieces that keep conversations on Sectr:

- `SectrAgent`: the AG-UI agent for a Sectr route. It's `@ag-ui/client`'s
  `HttpAgent`, plus reconnecting a reloaded page to its conversation.
- `SectrRunner`: the Copilot Runtime runner that keeps conversations on
  Sectr instead of in your server's memory.

```sh
npm install @sectr/copilotkit
```

It uses your app's own `@copilotkit/runtime` (1.77 or later), `@ag-ui/client`
and `rxjs`, which CopilotKit apps already have.

## Use it: through the Copilot Runtime (recommended)

```ts
// app/api/copilotkit/[[...slug]]/route.ts
import { CopilotRuntime, createCopilotRuntimeHandler } from "@copilotkit/runtime/v2";
import { SectrAgent, SectrRunner } from "@sectr/copilotkit";

const sectrAgent = new SectrAgent({ url: process.env.SECTR_AGUI_URL! });
const agents = { default: sectrAgent };
const runtime = new CopilotRuntime({ agents, runner: new SectrRunner({ agents }) });
const handler = createCopilotRuntimeHandler({ runtime, basePath: "/api/copilotkit" });

export const GET = handler;
export const POST = handler;
```

`SECTR_AGUI_URL` is the AG-UI URL that `sectr dev` and `sectr deploy` print
for your route. `SectrRunner` needs `SectrAgent`s in its agents map: it
reconnects through them.

## What it does

The Runtime's runner decides where conversations live. CopilotKit's default
keeps a copy of every thread in your server's memory and serves it back to
any caller. `SectrRunner` keeps nothing:

- **Runs** stream straight from Sectr: one request to your route's AG-UI
  endpoint, with the user's token (the Runtime forwards `Authorization`).
- **Reconnects** come from Sectr too, through the `SectrAgent`. A reloaded
  page gets the conversation so far, plus its latest run: still streaming if
  the agent is mid-turn, or ending in the approval that's waiting for a
  decision.
- **Errors** (a refused request, a broken stream) end the run with a
  `RUN_ERROR` instead of leaving the chat spinning.

Your server stays stateless, so deploy it anywhere: serverless, several
instances, restarts. The only copy of a conversation is the one Sectr shows
only to its owner.

## Use it: direct from the browser

For prototypes, give CopilotKit the agent itself, with no Runtime:

```tsx
import { SectrAgent } from "@sectr/copilotkit";

const agent = new SectrAgent({
  url: "https://api.sectr.dev/ag-ui/support-agent/chat",
  headers: { Authorization: `Bearer ${token}` },
});

<CopilotKitProvider selfManagedAgents={{ default: agent }}>
  <CopilotChat threadId={threadId} />
</CopilotKitProvider>
```

A reloaded page reconnects the same way: `SectrAgent` implements AG-UI's
`connect()`, which a plain `HttpAgent` leaves out, so with a plain
`HttpAgent` a reloaded page starts empty and a pending approval can't be
answered.

The full guide, including user tokens and approvals:
[Connecting your frontend](https://docs.sectr.dev/getting-started/connecting-frontend/).

## License

MIT
