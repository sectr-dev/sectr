"""Typed event model mirroring `schema/event.schema.json`.

Two layers live here:

- `RunnerEvent` subclasses — what user code (or a framework adapter) emits on
  the invoke response stream: `{type, payload}` only. The journal writer
  assigns `id`/`seq`/`created_at`/`session_id`/`deployment_id` when the
  sidecar appends. Serialization is validated against the schema's
  `$defs/runnerEvent` by the golden tests in `tests/`.
- `Envelope` — one journal row / client SSE frame (the schema root): a
  `RunnerEvent` plus the journal-writer-assigned fields. Transcript helpers
  consume these.

Platform-emitted types (SESSION_CREATED, INVOCATION_STARTED,
TOOL_APPROVAL_GRANTED/DENIED, SESSION_ERROR) have no constructors here —
runners never emit them; they only appear inside rehydrated transcripts
(as `Envelope`s).
"""

from __future__ import annotations

from dataclasses import MISSING, dataclass, fields
from typing import Any, ClassVar

__all__ = [
    "Envelope",
    "RunnerEvent",
    "MessageStart",
    "MessageDelta",
    "MessageEnd",
    "ToolCall",
    "ToolResult",
    "ToolApprovalRequired",
    "Error",
    "NoMoreActions",
    "TranscriptCompacted",
]


class RunnerEvent:
    """Base class for runner-emittable events (`$defs/runnerEvent`)."""

    type: ClassVar[str]

    def payload(self) -> dict[str, Any]:
        """Serialize dataclass fields to the payload dict. A field with a
        default is optional and omitted when None; a field without one is
        required and always emitted, even as None."""
        return {
            f.name: getattr(self, f.name)
            for f in fields(self)  # type: ignore[arg-type]
            if getattr(self, f.name) is not None
            or (f.default is MISSING and f.default_factory is MISSING)
        }

    def to_runner_event(self) -> dict[str, Any]:
        """The exact SSE frame body: `{type, payload}`."""
        return {"type": type(self).type, "payload": self.payload()}


@dataclass
class MessageStart(RunnerEvent):
    """Start of an assistant message."""

    type: ClassVar[str] = "MESSAGE_START"
    role: str
    message_id: str


@dataclass
class MessageDelta(RunnerEvent):
    """Content delta — interpretable, not opaque: ``{"type": "text" |
    "reasoning", "text": ...}``. The kind vocabulary is closed so the
    platform can interpret every journaled delta (AG-UI: ``text`` →
    TEXT_MESSAGE_CONTENT, ``reasoning`` → REASONING_MESSAGE_*); framework chunk
    types outside it must be normalized by the adapter before journaling
    (e.g. Anthropic ``input_json_delta`` accumulates into
    ``TOOL_CALL.arguments``)."""

    type: ClassVar[str] = "MESSAGE_DELTA"
    message_id: str
    delta: dict[str, Any]


@dataclass
class MessageEnd(RunnerEvent):
    """End of an assistant message. `framework_state` is opaque to the
    platform: what the adapter that wrote it needs to rebuild the message
    as its framework stored it (e.g. signed reasoning), read back only by
    that adapter."""

    type: ClassVar[str] = "MESSAGE_END"
    message_id: str
    finish_reason: str | None = None
    framework_state: dict[str, Any] | None = None


@dataclass
class ToolCall(RunnerEvent):
    """Journal record of a tool call initiated inside the runner. The
    platform never executes tools; `name` + `arguments` must suffice for a
    resumed runner to execute an approved call from journal data alone."""

    type: ClassVar[str] = "TOOL_CALL"
    tool_call_id: str
    name: str
    arguments: Any


@dataclass
class ToolResult(RunnerEvent):
    """Journaled after every successful tool execution — normal mid-turn
    calls and post-approval executions alike — and for a denied approval.
    Not guaranteed for every TOOL_CALL (a crash or a failing tool leaves
    none); `SessionContext.messages` then synthesizes the missing output."""

    type: ClassVar[str] = "TOOL_RESULT"
    tool_call_id: str
    result: Any


@dataclass
class ToolApprovalRequired(RunnerEvent):
    """Requests human approval. MUST be the final runner event of a turn;
    the turn is then suspended (no NO_MORE_ACTIONS follows)."""

    type: ClassVar[str] = "TOOL_APPROVAL_REQUIRED"
    tool_call_id: str
    name: str
    arguments: Any
    reason: str | None = None


@dataclass
class Error(RunnerEvent):
    """Runner-level error; ends the turn (the driver appends NO_MORE_ACTIONS)."""

    type: ClassVar[str] = "ERROR"
    message: str
    code: str | None = None
    transient: bool | None = None


@dataclass
class TranscriptCompacted(RunnerEvent):
    """Emitted when the agent framework compacts context (its algorithm, its
    model access — captured and journaled by the adapter mid-stream). The
    payload is the FULL post-compaction context in canonical message form:
    the fold in `SessionContext.messages` resets its accumulator to these
    messages and appends everything journaled after this event. The snapshot
    must not drop an unanswered tool call."""

    type: ClassVar[str] = "TRANSCRIPT_COMPACTED"
    replaces_up_to_seq: int
    messages: list[dict[str, Any]]


@dataclass
class NoMoreActions(RunnerEvent):
    """THE turn-ending sentinel: 'the agent has no pending actions — the ball
    is in the user's court.' The SDK driver emits it on generator exhaustion
    and after an unhandled error; raw runners must end their stream with it.
    NEVER emitted on an approval pause — a gated tool is a pending action, so
    the turn is suspended, not over. Raw-mode user code may yield it
    explicitly to end the turn."""

    type: ClassVar[str] = "NO_MORE_ACTIONS"


@dataclass
class Envelope:
    """One journal row / client SSE frame — the `event.schema.json` root."""

    id: str
    session_id: str
    deployment_id: str
    seq: int
    type: str
    payload: dict[str, Any]
    created_at: str

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Envelope:
        # TODO(phase-2): validate against event.schema.json (conformance).
        return cls(**data)
