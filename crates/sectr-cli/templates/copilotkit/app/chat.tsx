"use client";

import {
  CopilotSidebar,
  useDefaultRenderTool,
  useInterrupt,
} from "@copilotkit/react-core/v2";

// "default" is the agent in the Copilot Runtime's agents map
// (app/api/copilotkit): your Sectr agent's /chat route.
const AGENT_ID = "default";

export function Chat({ threadId }: { threadId: string }) {
  // Show the agent's tool calls (and their results) as cards in the chat.
  useDefaultRenderTool();

  // Tools marked `needs_approval` pause the agent on Sectr; the pause arrives
  // as an AG-UI interrupt. Approve or deny it right in the chat.
  useInterrupt({
    agentId: AGENT_ID,
    enabled: ({ value }) => (value as { reason?: string })?.reason === "tool_approval",
    render: ({ interrupt, resolve }) => (
      <div className="approval">
        <p>
          The agent wants to run <code>{String(interrupt?.metadata?.toolName)}</code>{" "}
          with <code>{JSON.stringify(interrupt?.metadata?.arguments)}</code>
        </p>
        <button onClick={() => resolve(true)}>Approve</button>
        <button onClick={() => resolve(false)}>Deny</button>
      </div>
    ),
  });

  return <CopilotSidebar agentId={AGENT_ID} threadId={threadId} defaultOpen />;
}
