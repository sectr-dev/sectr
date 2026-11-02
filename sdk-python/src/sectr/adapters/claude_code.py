"""Claude Code adapter (`sectr[claude-code]` extra) — the near-black-box
case: the CLI owns its loop, tools, and permissions.

`claude_code(**options)` returns a handler that (see
`examples/claude-code/main.py`):

- runs the `claude` CLI (via claude-code-sdk) with `--resume` keyed to our
  session_id (option (b): the CLI's workspace state persists across turns;
  the journal stays authoritative for approvals, SSE, audit, transcript);
- assistant text → MESSAGE_*; ToolUseBlock → TOOL_CALL; ToolResultBlock →
  TOOL_RESULT; result message → NO_MORE_ACTIONS (turn over);
- pause: the CLI's `can_use_tool` permission callback fires → the adapter
  captures (tool_name, input), terminates the subprocess, journals
  TOOL_APPROVAL_REQUIRED as the final frame (turn suspended, no sentinel);
- resume: `--resume` the same session; when the CLI re-issues the gated
  tool and asks permission, the adapter answers from
  `ctx.decision` (allow/deny). At most ONE pending approval per
  session is possible by the turn model.

CI note: the `claude` binary is a shim script emitting canned JSONL
(examples/claude-code/tests/shim_claude.py). Implementation pending.
"""

from __future__ import annotations

from collections.abc import AsyncIterator
from typing import Any

from ..app import Handler
from ..context import SessionContext
from ..events import RunnerEvent

__all__ = ["claude_code"]


def claude_code(**options: Any) -> Handler:
    """Handler factory wrapping claude-code-sdk options. Implementation
    pending."""

    async def handler(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
        raise NotImplementedError("sectr.adapters.claude_code")
        yield  # pragma: no cover - makes this an async generator

    return handler
