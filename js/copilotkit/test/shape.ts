// Type-level test (checked by `npm run typecheck`, never run): the runner
// plugs into CopilotRuntime exactly as the docs show, and SectrAgent is an
// HttpAgent anywhere one is expected.
import { HttpAgent } from "@ag-ui/client";
import { AgentRunner, CopilotRuntime } from "@copilotkit/runtime/v2";

import { SectrAgent, SectrRunner } from "../src/index.js";

const agents = { default: new SectrAgent({ url: "https://api.sectr.dev/ag-ui/app/route" }) };
const runner: AgentRunner = new SectrRunner({ agents });
new CopilotRuntime({ agents, runner });

const asHttpAgent: HttpAgent = agents.default;
void asHttpAgent;

// A plain HttpAgent can't reconnect, so SectrRunner refuses it.
// @ts-expect-error connectThread is missing
new SectrRunner({ agents: { default: new HttpAgent({ url: "https://api.sectr.dev/ag-ui/app/route" }) } });
