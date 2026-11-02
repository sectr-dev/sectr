import {
  CopilotRuntime,
  createCopilotRuntimeHandler,
} from "@copilotkit/runtime/v2";
import { SectrAgent, SectrRunner } from "@sectr/copilotkit";

// Your agent's /chat route on Sectr. SECTR_AGUI_URL is the AG-UI endpoint
// that `sectr dev` / `sectr deploy` prints. The browser's `Authorization`
// header (the user token from /api/agent-token) is forwarded to Sectr by the
// Runtime.
const agents = {
  default: new SectrAgent({ url: process.env.SECTR_AGUI_URL!.replace(/\/+$/, "") }),
};

// SectrRunner keeps this server stateless: conversations live on Sectr, and
// every read goes there with the user's token (@sectr/copilotkit). It gets
// the same agents map: a reloaded page reconnects through its SectrAgent.
const runtime = new CopilotRuntime({ agents, runner: new SectrRunner({ agents }) });

const handler = createCopilotRuntimeHandler({
  runtime,
  basePath: "/api/copilotkit",
});

export const GET = handler;
export const POST = handler;
export const PATCH = handler;
export const DELETE = handler;
