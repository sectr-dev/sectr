"""Support-agent tools — plain functions registered with sectr.

Framework-free variant: `sectr.tools.tool` is the whole story. Gating is
code-level (`needs_approval=True`) plus the deployment's policy
(`[deploy] approval_tools` in sectr.toml).
"""

from sectr.tools import tool


@tool
async def lookup_order(order_id: str) -> dict[str, str]:
    """Look up the status of an order."""
    return {"order_id": order_id, "status": "shipped"}


@tool(needs_approval=True)
async def refund(order_id: str) -> str:
    """Refund an order (requires manager approval)."""
    return f"Refunded {order_id}"


@tool
async def todo_write(items: list[str]) -> str:
    """Replace the todo list. The journaled tool call IS the write: state
    lives in the transcript (the model reads its history directly). Reading
    it back programmatically is a post-MVP SDK feature."""
    return "ok"


TOOLS = [lookup_order, refund, todo_write]
