"""Sectr Python SDK: framework-agnostic agent deployment.

Core (framework-free): `AgentApp` (route registry), `SessionContext` (the
handler argument), `sectr.events` (typed events mirroring
schema/event.schema.json), `sectr.tools` (`@tool` registry + approval
gating). Handlers are async generators of events; the FastAPI server
adapter (`sectr.server`) speaks the runner invoke contract, and framework
adapters (`sectr.adapters.*`) wrap native framework objects into handlers.

Contracts: `../schema/` is the single source of truth — see `schema/README.md`.
"""

from .app import AgentApp, Handler, Route
from .context import SessionContext
from .events import (
    Envelope,
    Error,
    MessageDelta,
    MessageEnd,
    MessageStart,
    NoMoreActions,
    RunnerEvent,
    TranscriptCompacted,
    ToolApprovalRequired,
    ToolCall,
    ToolResult,
    NoMoreActions,
)
from .tools import ApprovalRequired, Tool, tool

__version__ = "0.1.0"

__all__ = [
    "AgentApp",
    "ApprovalRequired",
    "Envelope",
    "Error",
    "Handler",
    "MessageDelta",
    "MessageEnd",
    "MessageStart",
    "Route",
    "RunnerEvent",
    "SessionContext",
    "Tool",
    "ToolApprovalRequired",
    "ToolCall",
    "ToolResult",
    "NoMoreActions",
    "TranscriptCompacted",
    "tool",
]
