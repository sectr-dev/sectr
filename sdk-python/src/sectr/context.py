"""SessionContext — the single argument every handler receives.

Handlers are async generators of `sectr.events.RunnerEvent`; the adapter
(FastAPI server or a framework adapter) constructs the context from the
invoke request, drives the generator via `sectr.drive` (finality + the
NO_MORE_ACTIONS sentinel), and journals the stream.
"""

from __future__ import annotations

from typing import Any

from .events import Envelope
from .tools import ApprovalRequired, get_tool, is_gated

__all__ = ["SessionContext", "INTERRUPTED_TOOL_RESULT"]

# The synthesized answer the fold gives a tool call that never got a
# TOOL_RESULT but was followed by more conversation (crash, or a tool that
# raised). Derived from the journal, never persisted.
INTERRUPTED_TOOL_RESULT = "This tool call did not complete; no result was recorded."


class SessionContext:
    """One invocation: rehydrated transcript + the polymorphic reason
    (user_message | approval_decision | tool_result[post-MVP]). The
    transcript already includes this invocation's INVOCATION_STARTED, so
    `messages` already contains the current user message."""

    def __init__(
        self,
        transcript: list[Envelope],
        reason: dict[str, Any],
        *,
        session_id: str | None = None,
        invocation_id: str | None = None,
    ) -> None:
        self.transcript = transcript
        self.reason = reason
        self.session_id = session_id
        self.invocation_id = invocation_id

    # ------------------------------------------------------------------
    # Reason sugars — raise on the wrong kind so misuse fails loudly
    # ------------------------------------------------------------------

    @property
    def input(self) -> str:
        """The user's message text (user_message turns only)."""
        if self.reason.get("kind") != "user_message":
            raise TypeError(
                f"turn reason is {self.reason.get('kind')!r}, not 'user_message'"
            )
        return self.reason["input"]

    @property
    def decision(self) -> dict[str, Any]:
        """The approval decision (approval_decision turns only):
        `{"approved": bool, "approver"?: str}`."""
        if self.reason.get("kind") != "approval_decision":
            raise TypeError(
                f"turn reason is {self.reason.get('kind')!r}, not 'approval_decision'"
            )
        return self.reason["decision"]

    # ------------------------------------------------------------------
    # Approval resume helpers
    # ------------------------------------------------------------------

    @property
    def pending_tool_call(self) -> dict[str, Any] | None:
        """The unanswered tool call on an approval-resume turn: the payload
        of the last TOOL_CALL / TOOL_APPROVAL_REQUIRED envelope that has no
        subsequent TOOL_RESULT for the same `tool_call_id`. A later
        user_message turn abandons it: the fold answers such a call as
        interrupted (`INTERRUPTED_TOOL_RESULT`), so it can no longer be
        resumed. Mirrors `sectr_common::journal::pending_tool_call`."""

        pending: dict[str, Any] | None = None
        for event in self.transcript:
            if event.type in ("TOOL_CALL", "TOOL_APPROVAL_REQUIRED"):
                pending = dict(event.payload)
            elif (
                event.type == "INVOCATION_STARTED"
                and (event.payload.get("reason") or {}).get("kind") == "user_message"
            ):
                pending = None
            elif (
                event.type == "TOOL_RESULT"
                and pending is not None
                and event.payload.get("tool_call_id") == pending.get("tool_call_id")
            ):
                pending = None
        return pending

    # ------------------------------------------------------------------
    # Tool execution
    # ------------------------------------------------------------------

    async def execute(
        self,
        name: str,
        *,
        call_id: str | None = None,
        reason: str | None = None,
        **arguments: Any,
    ) -> Any:
        """Execute a registered tool. Raises `ApprovalRequired` when the
        tool is gated (code marking or platform policy) and no decision has
        been made — the adapter converts that into the turn-suspending
        TOOL_APPROVAL_REQUIRED event. On an approval-resume turn, call this
        for the pending tool with the decision already in hand
        (`ctx.decision`); it executes normally."""
        # Gated tools raise on a user_message turn (the adapter converts
        # that into the turn-suspending TOOL_APPROVAL_REQUIRED). On an
        # approval_decision turn the decision is already in hand — the
        # documented contract is that executing the PENDING tool proceeds
        # (found missing 2026-09-14: the gate re-raised, so approval
        # resumes re-suspended forever).
        if is_gated(name) and self.reason.get("kind") != "approval_decision":
            raise ApprovalRequired(name, arguments, call_id=call_id, reason=reason)
        return await get_tool(name)(**arguments)

    async def request_approval(
        self,
        name: str,
        arguments: Any,
        *,
        call_id: str | None = None,
        reason: str | None = None,
    ) -> None:
        """Explicit approval-request sugar: always raises `ApprovalRequired`
        (the adapter catches it and ends the turn). Use when the approval
        target is not a registered tool call — e.g. an action the agent is
        about to take that no single tool expresses."""
        raise ApprovalRequired(name, arguments, call_id=call_id, reason=reason)

    # ------------------------------------------------------------------
    # Transcript views (framework-shaped)
    # ------------------------------------------------------------------

    @property
    def messages(self) -> list[dict[str, Any]]:
        """Canonical transcript view: OpenAI-style message dicts covering the
        full multi-turn conversation.

        - user turns come from `INVOCATION_STARTED` envelopes whose reason is
          `user_message` (the current user message is included — the
          transcript already contains this invocation's start event)
        - assistant messages from MESSAGE_START/DELTA/END (deltas merged in
          order: typed `text` deltas append to `content`, typed `reasoning`
          deltas accumulate under `reasoning_content`; anything else lands
          under `_sectr_native_chunks` — defensive, for journals written
          before the delta vocabulary was closed, which also allowed bare
          strings)
        - a `MESSAGE_END`'s `framework_state` (opaque, adapter-owned) lands
          on its message under `_sectr_framework_state`, for the adapter
          that wrote it to rebuild the message as its framework stored it
        - `TOOL_CALL` envelopes attach to the current invocation's latest
          assistant message as OpenAI `tool_calls` (arguments JSON-encoded),
          but only while it is still the last message; otherwise (a
          tool-only response, e.g. after a tool result or at the start of a
          turn) the call gets a new empty assistant message. Parallel calls
          still share one message: their results follow all of them.
        - `TOOL_RESULT` envelopes become `{role: "tool"}` messages
        - an unanswered tool call at the end of the transcript (approval
          pause) is preserved exactly as the LLM left it — the resumed turn
          appends the tool result message
        - an unanswered tool call FOLLOWED by more conversation (the turn
          crashed or the tool raised before a TOOL_RESULT, and the session
          moved on) gets a synthesized `{role: "tool"}` answer,
          `INTERRUPTED_TOOL_RESULT`, right after its call's results —
          derived from the journal, never persisted. Every prefix of a
          journal is a valid journal (a crash can cut it anywhere), so the
          fold must turn any prefix into a conversation providers accept.

        Platform lifecycle events without conversation content
        (SESSION_CREATED, INVOCATION_STARTED, APPROVAL_*, ...) contribute nothing.
        Framework adapters project this canonical view into their native
        state; claude-code renders it as text instead.

        Crash-mid-turn continuation (see `schema/README.md`): a
        trailing assistant message that never received MESSAGE_END is
        emitted as-is with its partial content, followed by a SYNTHESIZED
        user message asking the model to continue — deterministic from the
        journal, never persisted (the same derivation rule user turns
        already follow via INVOCATION_STARTED). Only unterminated MESSAGES
        stitch; an unanswered TOOL_CALL is answered by the rule above once
        the conversation continues past it.
        """
        import json

        msgs: list[dict[str, Any]] = []
        open_by_id: dict[str, dict[str, Any]] = {}
        current: dict[str, Any] | None = None
        # Crash-stitch tracking: any message opened but never ended,
        # and whether a SESSION_ERROR followed it (=> the turn died under
        # it; the next invocation continues with a synthesized prompt
        # inserted AT the point the turn died — before any later events'
        # derived messages).
        unterminated = False
        session_error_after = False
        stitch_at: int | None = None

        for event in self.transcript:
            payload = event.payload
            etype = event.type

            if etype == "TRANSCRIPT_COMPACTED":
                # Positional reset: the snapshot IS the context so far. All
                # merge state (open assistant messages, current-pointer)
                # restarts with it — including the crash-stitch tracking
                # (a compaction snapshot is closed context by definition).
                msgs = [dict(m) for m in payload["messages"]]
                open_by_id = {}
                unterminated = False
                session_error_after = False
                stitch_at = None
                current = None
                continue

            if etype == "INVOCATION_STARTED":
                # A new turn never inherits the previous turn's message
                # (the AG-UI fold in sectr-common does the same).
                current = None
                reason = payload.get("reason") or {}
                if reason.get("kind") == "user_message":
                    msgs.append({"role": "user", "content": reason["input"]})

            elif etype == "MESSAGE_START":
                msg: dict[str, Any] = {
                    "role": payload.get("role", "assistant"),
                    "content": "",
                }
                open_by_id[payload["message_id"]] = msg
                current = msg
                msgs.append(msg)
                unterminated = True
                session_error_after = False

            elif etype == "MESSAGE_DELTA":
                existing: dict[str, Any] | None = open_by_id.get(payload["message_id"])
                if existing is None:
                    continue  # delta without a START: defensive
                msg = existing
                delta = payload["delta"]
                if isinstance(delta, dict) and delta.get("type") == "text":
                    msg["content"] += delta.get("text", "")
                elif isinstance(delta, dict) and delta.get("type") == "reasoning":
                    msg["reasoning_content"] = (
                        msg.get("reasoning_content", "") + delta.get("text", "")
                    )
                else:
                    # Foreign chunk (incl. bare strings from pre-closure
                    # journals): preserved verbatim — the schema's closed
                    # delta vocabulary rejects them for new writes.
                    msg.setdefault("_sectr_native_chunks", []).append(delta)

            elif etype == "TOOL_CALL":
                arguments = payload["arguments"]
                call = {
                    "id": payload["tool_call_id"],
                    "type": "function",
                    "function": {
                        "name": payload["name"],
                        "arguments": (
                            arguments if isinstance(arguments, str) else json.dumps(arguments)
                        ),
                    },
                }
                if current is None or not msgs or msgs[-1] is not current:
                    current = {"role": "assistant", "content": ""}
                    msgs.append(current)
                current.setdefault("tool_calls", []).append(call)

            elif etype == "MESSAGE_END":
                ended = open_by_id.pop(payload["message_id"], None)
                if ended is not None and payload.get("framework_state") is not None:
                    ended["_sectr_framework_state"] = payload["framework_state"]
                if not open_by_id:
                    unterminated = False

            elif etype == "SESSION_ERROR":
                # The reconciler / engine synthesized this — an abnormal
                # turn end. If a message was open when it landed, the next
                # invocation continues with the stitch (inserted at this
                # point in the fold, before any later-derived messages).
                session_error_after = True
                stitch_at = len(msgs)

            elif etype == "TOOL_RESULT":
                result = payload["result"]
                msgs.append(
                    {
                        "role": "tool",
                        "tool_call_id": payload["tool_call_id"],
                        "content": (
                            result if isinstance(result, str) else json.dumps(result)
                        ),
                    }
                )

        if unterminated and session_error_after and stitch_at is not None:
            msgs.insert(
                stitch_at,
                {
                    "role": "user",
                    "content": (
                        "An error interrupted the generation of your previous "
                        "message. Please continue from where it left off."
                    ),
                },
            )

        return _answer_interrupted_calls(msgs)


def _answer_interrupted_calls(msgs: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Give every tool call that was never answered, but is followed by
    more conversation, a synthesized tool message placed right after its
    call's existing results. A call with nothing after it is left alone:
    that is a suspended approval (or the turn now resuming it)."""
    answered = {m.get("tool_call_id") for m in msgs if m.get("role") == "tool"}
    out: list[dict[str, Any]] = []
    i = 0
    while i < len(msgs):
        msg = msgs[i]
        out.append(msg)
        i += 1
        calls = msg.get("tool_calls") if msg.get("role") == "assistant" else None
        if not calls:
            continue
        while i < len(msgs) and msgs[i].get("role") == "tool":
            out.append(msgs[i])
            i += 1
        if i == len(msgs):
            continue  # trailing: an approval pause owns these calls
        out.extend(
            {"role": "tool", "tool_call_id": call["id"], "content": INTERRUPTED_TOOL_RESULT}
            for call in calls
            if call["id"] not in answered
        )
    return out
