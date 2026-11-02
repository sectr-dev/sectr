"""Tests for the compaction fold in SessionContext.messages: snapshot resets
the accumulator, tail appends after it, and no snapshots means identity."""

from __future__ import annotations

from sectr import SessionContext
from sectr.events import Envelope

REASON = {"kind": "user_message", "input": {}}


def env(seq: int, type: str, payload: dict) -> Envelope:  # noqa: A002
    return Envelope(id=f"e{seq}", session_id="s", deployment_id="d", seq=seq,
                    type=type, payload=payload, created_at="2026-01-01T00:00:00Z")


def test_snapshot_resets_accumulator_and_tail_appends() -> None:
    transcript = [
        env(0, "INVOCATION_STARTED", {"invocation_id": "i1", "route": "/chat", "deployment_id": "d",
            "reason": {"kind": "user_message", "input": "hi"}}),
        env(1, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(2, "MESSAGE_END", {"message_id": "m1"}),
        env(3, "TOOL_CALL", {"tool_call_id": "c1", "name": "t", "arguments": {}}),
        env(4, "TOOL_RESULT", {"tool_call_id": "c1", "result": "r"}),
        env(5, "TRANSCRIPT_COMPACTED", {"replaces_up_to_seq": 4,
            "messages": [{"role": "user", "content": "summary of everything before"}]}),
        env(6, "MESSAGE_START", {"role": "assistant", "message_id": "m2"}),
        env(7, "MESSAGE_DELTA", {"message_id": "m2", "delta": {"type": "text", "text": "answer"}}),
        env(8, "MESSAGE_END", {"message_id": "m2"}),
    ]
    assert SessionContext(transcript=transcript, reason=REASON).messages == [
        {"role": "user", "content": "summary of everything before"},
        {"role": "assistant", "content": "answer"},
    ]


def test_multiple_snapshots_last_wins() -> None:
    transcript = [
        env(0, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(1, "TRANSCRIPT_COMPACTED", {"replaces_up_to_seq": 0,
            "messages": [{"role": "user", "content": "first summary"}]}),
        env(2, "MESSAGE_START", {"role": "assistant", "message_id": "m2"}),
        env(3, "TRANSCRIPT_COMPACTED", {"replaces_up_to_seq": 2,
            "messages": [{"role": "user", "content": "second summary"}]}),
        env(4, "MESSAGE_START", {"role": "assistant", "message_id": "m3"}),
    ]
    assert SessionContext(transcript=transcript, reason=REASON).messages == [
        {"role": "user", "content": "second summary"},
        {"role": "assistant", "content": ""},
    ]


def test_merge_state_restarts_after_snapshot() -> None:
    # an assistant message opened BEFORE the snapshot must not absorb a
    # tool call journaled AFTER it
    transcript = [
        env(0, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(1, "TRANSCRIPT_COMPACTED", {"replaces_up_to_seq": 0, "messages": []}),
        env(2, "TOOL_CALL", {"tool_call_id": "c1", "name": "t", "arguments": {}}),
    ]
    msgs = SessionContext(transcript=transcript, reason=REASON).messages
    assert msgs == [
        {"role": "assistant", "content": "",
         "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "t", "arguments": "{}"}}]},
    ]


def test_snapshot_is_transparent_to_pending_tool_call() -> None:
    transcript = [
        env(0, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(1, "TRANSCRIPT_COMPACTED", {"replaces_up_to_seq": 0,
            "messages": [{"role": "user", "content": "summary"}]}),
        env(2, "TOOL_CALL", {"tool_call_id": "c1", "name": "refund", "arguments": {}}),
        env(3, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c1", "name": "refund", "arguments": {}}),
    ]
    ctx = SessionContext(transcript=transcript, reason=REASON)
    assert ctx.messages[-1]["tool_calls"][0]["id"] == "c1"  # still open — LLM left it that way
    assert ctx.pending_tool_call["tool_call_id"] == "c1"
