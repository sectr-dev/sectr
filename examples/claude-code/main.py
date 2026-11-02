"""Claude Code variant — the support agent via the `claude` CLI
(claude-code-sdk), the near-black-box case: the CLI owns its loop, tools,
and permissions.

The adapter runs the CLI with --resume keyed to our session_id, maps its message stream onto
platform events, and turns permission prompts into a turn-SUSPENDING
TOOL_APPROVAL_REQUIRED (the final frame; no NO_MORE_ACTIONS) — answering
them from the platform decision on resume turns.

Custom tools (lookup_order, refund) are exposed via an MCP server; todo
state uses the CLI's built-in tool.

CI: the `claude` binary is a shim script emitting canned JSONL
(tests/shim_claude.py — an executable spec of the subprocess protocol).
"""

from sectr import AgentApp
from sectr.adapters.claude_code import claude_code

from tools import support_mcp_server

app = AgentApp(name="support-agent")
app.add_route(
    "/chat",
    claude_code(
        mcp_servers={"support": support_mcp_server},
        allowed_tools=[
            "mcp__support__lookup_order",
            "mcp__support__refund",
        ],
    ),
)
