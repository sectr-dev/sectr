"""Support-agent tools — plain functions, no framework or sectr imports.

One useful tool (lookup_order) + one gated tool (refund). app.py registers
them on the Agent and marks `refund` with `requires_approval=True`
(Pydantic AI's native deferred tools), which is what makes it pause for a
human approval.
"""


async def lookup_order(order_id: str) -> dict[str, str]:
    """Look up the status of an order."""
    return {"order_id": order_id, "status": "shipped"}


async def refund(order_id: str) -> str:
    """Refund an order (requires manager approval)."""
    return f"Refunded {order_id}"
