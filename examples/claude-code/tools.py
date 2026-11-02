"""Support-agent CUSTOM tools for the Claude Code variant: exposed to the
CLI as an MCP server (FastMCP). Todo state uses the CLI's built-in tool, so
it is not declared here. Approval is handled by the adapter via the CLI's
permission flow (can_use_tool), consulting the platform decision on resume
turns."""

from fastmcp import FastMCP

support_mcp_server = FastMCP("support")


@support_mcp_server.tool()
async def lookup_order(order_id: str) -> dict[str, str]:
    """Look up the status of an order."""
    return {"order_id": order_id, "status": "shipped"}


@support_mcp_server.tool()
async def refund(order_id: str) -> str:
    """Refund an order (requires manager approval)."""
    return f"Refunded {order_id}"

