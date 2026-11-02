"""Calibrate the mock's request validator (`src/mock/validate.rs`): which
malformed Chat Completions requests do the real APIs reject?

Sends each shape below to OpenAI directly and to OpenRouter (routed to OpenAI
and to Claude), and prints one JSON row per shape. Costs a fraction of a
cent. Keys come from the environment or the given .env file:

    python calibrate/validator.py ../../.env results.json

Last run 2026-10-10 (gpt-5.4-nano, openai/gpt-5.4-nano,
anthropic/claude-haiku-5.5); the rules and their unit test mirror its
results.
"""

import json
import os
import sys

import httpx

ENV = sys.argv[1]
OUT = sys.argv[2]


def env_key(name):
    if os.environ.get(name):
        return os.environ[name]
    for line in open(ENV):
        if line.startswith(name + "="):
            return line.split("=", 1)[1].strip().strip("\"'")
    raise SystemExit(f"{name} not found")


TARGETS = {
    "openai": ("https://api.openai.com/v1/chat/completions", env_key("OPENAI_API_KEY"), "gpt-5.4-nano", "max_completion_tokens"),
    "or-openai": ("https://openrouter.ai/api/v1/chat/completions", env_key("OPENROUTER_API_KEY"), "openai/gpt-5.4-nano", "max_tokens"),
    "or-claude": ("https://openrouter.ai/api/v1/chat/completions", env_key("OPENROUTER_API_KEY"), "anthropic/claude-haiku-5.5", "max_tokens"),
}

TOOLS = [{
    "type": "function",
    "function": {
        "name": "lookup_order",
        "description": "Look up an order's status.",
        "parameters": {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]},
    },
}]


def call(cid, args='{"order_id": "42"}'):
    return {"id": cid, "type": "function", "function": {"name": "lookup_order", "arguments": args}}


U = {"role": "user", "content": "Where is order 42?"}
U2 = {"role": "user", "content": "Thanks. Anything else?"}


def asst(*calls, content=None):
    m = {"role": "assistant", "content": content}
    if calls:
        m["tool_calls"] = list(calls)
    return m


def tool(cid, content="shipped"):
    return {"role": "tool", "tool_call_id": cid, "content": content}


VARIANTS = {
    "valid: call then result": [U, asst(call("c1")), tool("c1")],
    "valid: text + call then result, then user": [U, asst(call("c1"), content="Checking."), tool("c1"), asst(content="Shipped."), U2],
    "unanswered call, then user": [U, asst(call("c1")), U2],
    "unanswered call at the end": [U, asst(call("c1"))],
    "tool result for an unknown id": [U, asst(call("c1")), tool("c9")],
    "tool result with no assistant call": [U, tool("c1")],
    "duplicate call ids": [U, asst(call("c1"), call("c1", '{"order_id": "43"}')), tool("c1"), tool("c1")],
    "arguments not JSON": [U, asst(call("c1", "not json")), tool("c1")],
    "arguments a JSON array": [U, asst(call("c1", "[1, 2]")), tool("c1")],
    "arguments empty string": [U, asst(call("c1", "")), tool("c1")],
    "empty assistant message (content '')": [U, asst(content=""), U2],
    "empty assistant message (content null)": [U, asst(), U2],
    "trailing assistant text": [U, asst(content="The order")],
    "two results out of call order": [U, asst(call("c1"), call("c2", '{"order_id": "43"}')), tool("c2"), tool("c1")],
    "one of two calls unanswered, then user": [U, asst(call("c1"), call("c2", '{"order_id": "43"}')), tool("c1"), U2],
    "two consecutive user messages": [U, U2],
    "tool content as text parts": [U, asst(call("c1")), tool("c1", [{"type": "text", "text": "shipped"}])],
    "tool content a bare JSON list": [U, asst(call("c1")), tool("c1", ["socks", "hat"])],
    "assistant with extra reasoning field": [U, {**asst(call("c1")), "reasoning": "thinking"}, tool("c1")],
}

rows = []
with httpx.Client(timeout=120) as client:
    for name, messages in VARIANTS.items():
        row = {"variant": name}
        for tname, (url, key, model, max_param) in TARGETS.items():
            body = {"model": model, "messages": messages, "tools": TOOLS, max_param: 300}
            r = client.post(url, headers={"Authorization": f"Bearer {key}"}, json=body)
            try:
                j = r.json()
            except Exception:
                j = {"error": {"message": r.text[:200]}}
            if "error" in j:
                msg = j["error"].get("message", "")
                raw = (j["error"].get("metadata") or {}).get("raw")
                if raw:
                    msg += " | raw: " + str(raw)
                row[tname] = f"{r.status_code} {msg[:300]}"
            else:
                row[tname] = f"{r.status_code} ok"
        rows.append(row)
        print(json.dumps(row), flush=True)
json.dump(rows, open(OUT, "w"), indent=1)
