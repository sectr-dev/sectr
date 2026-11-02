"""Support-agent tools — framework-free sectr tools.

One useful tool (lookup_order) +
one gated tool (refund, via Sectr's native HITL `needs_approval`).

Why these import sectr: LangGraph has no per-tool approval without its
checkpointer, and Sectr runs graphs without one (the journal is the
checkpoint). So the tools are sectr tools, converted to LangChain tools in
app.py with `as_langgraph_tool`, which provides the gating: a gated call
pauses the turn, and once the approval lands the adapter runs the call.
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


TOOLS = [lookup_order, refund]
