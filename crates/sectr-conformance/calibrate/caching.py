"""Calibrate the replay check (`src/replay.rs`): which differences between
a request's earlier history and the same history replayed later actually
cost a prompt-cache hit on the real APIs?

For each variant, a conversation with a long cacheable prefix, then the
varied message, then another long stretch: sent as the first turn would
send it (A) twice, warming the cache and then measuring it, then as a
replay would send it (B). If B's cached tokens fall to about the prefix
before the varied message, the difference changes the prompt; if they
match A's, the provider doesn't see it. Costs a fraction of a cent. Keys
come from the environment or the given .env file:

    python calibrate/caching.py ../../.env

Caching is best-effort: a variant whose control (A's second send) caches
nothing is inconclusive, and is worth rerunning.

Last run 2026-10-12 (gpt-5.4-nano, claude-haiku-5-5), twice, with the same
results. A cache miss at the difference: a tool call's arguments in another
key order (Chat Completions, Responses, Anthropic's `tool_use.input`), an
assistant message's `name` dropped, an assistant message's text and tool
calls split into two messages. The same prompt: the arguments' spacing
(Chat Completions, Responses), a tool result as text parts or a string
(Chat Completions, Anthropic), Responses items' `id` and `status` dropped,
an assistant `output_text` part sent as a string, a Responses message
without `type` (its default, `message`). The replay check
(`src/replay.rs`) ignores exactly the latter.
"""

import json
import os
import sys
import uuid

import httpx

ENV = sys.argv[1]
OPENAI_MODEL = "gpt-5.4-nano"
ANTHROPIC_MODEL = "claude-haiku-5-5"


def env_key(name):
    if os.environ.get(name):
        return os.environ[name]
    for line in open(ENV):
        if line.startswith(name + "="):
            return line.split("=", 1)[1].strip().strip("\"'")
    raise SystemExit(f"{name} not found")


def long_text(tag, lines=170):
    """About 2,300 tokens, unique to `tag`."""
    carriers = ["DHL", "UPS", "FedEx", "USPS", "GLS"]
    return "\n".join(
        f"Record {tag}-{i}: order {1000 + i} shipped via {carriers[i % 5]} on day {i % 28 + 1}, "
        f"weight {i * 7 % 50} kg, status {'delivered' if i % 3 else 'in transit'}."
        for i in range(lines)
    )


ASK = "Refund order 42, it came late."
SAY = "Let me look."
CALL = "call_cache_1"
TOOL_NAME = "refund_order"
COMPACT = '{"order_id":"42","reason":"late"}'
SPACED = '{"order_id": "42", "reason": "late"}'
REORDERED = '{"reason":"late","order_id":"42"}'

FUNCTION = {
    "name": TOOL_NAME,
    "description": "Refund an order.",
    "parameters": {
        "type": "object",
        "properties": {"order_id": {"type": "string"}, "reason": {"type": "string"}},
        "required": ["order_id", "reason"],
    },
}

client = httpx.Client(timeout=120)


# ---- Chat Completions -------------------------------------------------------

def chat_call(args):
    return {"id": CALL, "type": "function", "function": {"name": TOOL_NAME, "arguments": args}}


def chat_messages(tag, assistant, tool_content="Refunded 42"):
    return [
        {"role": "system", "content": f"[{tag}] You are a support agent.\n" + long_text(tag + "a")},
        {"role": "user", "content": ASK},
        *assistant,
        {"role": "tool", "tool_call_id": CALL, "content": tool_content},
        {"role": "user", "content": long_text(tag + "b") + "\nSummarize in one word."},
    ]


def one_assistant(args=COMPACT, **extra):
    return [{"role": "assistant", "content": SAY, "tool_calls": [chat_call(args)], **extra}]


CHAT_VARIANTS = {
    "control": (one_assistant(), one_assistant()),
    "arguments spacing": (one_assistant(COMPACT), one_assistant(SPACED)),
    "arguments key order": (one_assistant(REORDERED), one_assistant(COMPACT)),
    "assistant name dropped": (one_assistant(name="lookup"), one_assistant()),
    "text and call split in two messages": (
        one_assistant(),
        [{"role": "assistant", "content": SAY},
         {"role": "assistant", "content": None, "tool_calls": [chat_call(COMPACT)]}],
    ),
}
CHAT_TOOL_VARIANTS = {
    "tool result as text parts": ("Refunded 42", [{"type": "text", "text": "Refunded 42"}]),
}


def chat(messages, tag):
    body = {"model": OPENAI_MODEL, "messages": messages, "tools": [{"type": "function", "function": FUNCTION}],
            "max_completion_tokens": 64, "prompt_cache_key": tag}
    r = client.post("https://api.openai.com/v1/chat/completions",
                    headers={"Authorization": f"Bearer {env_key('OPENAI_API_KEY')}"}, json=body)
    j = r.json()
    if "error" in j:
        return None, f"{r.status_code} {j['error'].get('message', '')[:200]}"
    usage = j["usage"]
    return (usage["prompt_tokens"], usage.get("prompt_tokens_details", {}).get("cached_tokens", 0)), None


# ---- Responses ---------------------------------------------------------------

def message_item(ids=True, parts=True):
    content = [{"type": "output_text", "text": SAY, "annotations": []}] if parts else SAY
    item = {"type": "message", "role": "assistant", "content": content}
    if ids:
        item |= {"id": "msg_" + uuid.uuid4().hex, "status": "completed"}
    return item


def call_item(args=COMPACT, ids=True):
    item = {"type": "function_call", "call_id": CALL, "name": TOOL_NAME, "arguments": args}
    if ids:
        item |= {"id": "fc_" + uuid.uuid4().hex, "status": "completed"}
    return item


def responses_input(tag, items):
    return [
        {"role": "user", "content": ASK},
        *items,
        {"type": "function_call_output", "call_id": CALL, "output": "Refunded 42"},
        {"role": "user", "content": long_text(tag + "b") + "\nSummarize in one word."},
    ]


def responses_variants():
    msg, call = message_item(), call_item()
    return {
        "control": ([msg, call], [msg, call]),
        "arguments spacing": ([message_item(False), call_item(COMPACT, False)],
                              [message_item(False), call_item(SPACED, False)]),
        "arguments key order": ([message_item(False), call_item(REORDERED, False)],
                                [message_item(False), call_item(COMPACT, False)]),
        "item id and status dropped": ([msg, call], [message_item(False), call_item(ids=False)]),
        "output_text parts as a string": ([message_item(False), call_item(ids=False)],
                                          [message_item(False, parts=False), call_item(ids=False)]),
        "message type omitted": ([message_item(False, parts=False), call_item(ids=False)],
                                 [{"role": "assistant", "content": SAY}, call_item(ids=False)]),
    }


def responses(input_items, tag):
    body = {"model": OPENAI_MODEL, "instructions": f"[{tag}] You are a support agent.\n" + long_text(tag + "a"),
            "input": input_items, "tools": [{"type": "function", **FUNCTION}], "max_output_tokens": 64,
            "store": False, "prompt_cache_key": tag}
    r = client.post("https://api.openai.com/v1/responses",
                    headers={"Authorization": f"Bearer {env_key('OPENAI_API_KEY')}"}, json=body)
    j = r.json()
    if j.get("error"):
        return None, f"{r.status_code} {j['error'].get('message', '')[:200]}"
    usage = j["usage"]
    return (usage["input_tokens"], usage.get("input_tokens_details", {}).get("cached_tokens", 0)), None


# ---- Anthropic -----------------------------------------------------------------

def anthropic_messages(tag, input_order, tool_content):
    return [
        {"role": "user", "content": ASK},
        {"role": "assistant", "content": [
            {"type": "text", "text": SAY},
            {"type": "tool_use", "id": "toolu_cache_1", "name": TOOL_NAME, "input": input_order},
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_cache_1", "content": tool_content},
            {"type": "text", "text": long_text(tag + "b") + "\nSummarize in one word.",
             "cache_control": {"type": "ephemeral"}},
        ]},
    ]


ORDERED = {"order_id": "42", "reason": "late"}
REVERSED = {"reason": "late", "order_id": "42"}
ANTHROPIC_VARIANTS = {
    "control": ((ORDERED, "Refunded 42"), (ORDERED, "Refunded 42")),
    "tool_use input key order": ((REVERSED, "Refunded 42"), (ORDERED, "Refunded 42")),
    "tool result as text parts": ((ORDERED, "Refunded 42"), (ORDERED, [{"type": "text", "text": "Refunded 42"}])),
}


def anthropic(messages, tag):
    body = {"model": ANTHROPIC_MODEL, "max_tokens": 64,
            "system": [{"type": "text", "text": f"[{tag}] You are a support agent.\n" + long_text(tag + "a"),
                        "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": TOOL_NAME, "description": FUNCTION["description"],
                       "input_schema": FUNCTION["parameters"]}],
            "messages": messages}
    r = client.post("https://api.anthropic.com/v1/messages",
                    headers={"x-api-key": env_key("ANTHROPIC_API_KEY"), "anthropic-version": "2023-06-01"},
                    json=body)
    j = r.json()
    if j.get("type") == "error":
        return None, f"{r.status_code} {j['error'].get('message', '')[:200]}"
    u = j["usage"]
    total = u["input_tokens"] + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)
    return (total, u.get("cache_read_input_tokens", 0)), None


# ---- run --------------------------------------------------------------------------

def measure(endpoint, variant, send, build_a, build_b):
    tag = uuid.uuid4().hex[:12]
    row = {"endpoint": endpoint, "variant": variant}
    results = []
    for build in (build_a, build_a, build_b):
        usage, error = send(build(tag), tag)
        if error:
            row["error"] = error
            print(json.dumps(row), flush=True)
            return
        results.append(usage)
    (_, _), (total, control), (_, replayed) = results
    row |= {"prompt": total, "cached_control": control, "cached_replay": replayed}
    if control == 0:
        row["verdict"] = "inconclusive (control cached nothing)"
    elif replayed >= control - 128:
        row["verdict"] = "same prompt"
    else:
        row["verdict"] = "cache miss at the difference"
    print(json.dumps(row), flush=True)


for name, (a, b) in CHAT_VARIANTS.items():
    measure("chat", name, chat, lambda t, a=a: chat_messages(t, a), lambda t, b=b: chat_messages(t, b))
for name, (a, b) in CHAT_TOOL_VARIANTS.items():
    measure("chat", name, chat, lambda t, a=a: chat_messages(t, one_assistant(), a),
            lambda t, b=b: chat_messages(t, one_assistant(), b))
for name, (a, b) in responses_variants().items():
    measure("responses", name, responses, lambda t, a=a: responses_input(t, a),
            lambda t, b=b: responses_input(t, b))
for name, (a, b) in ANTHROPIC_VARIANTS.items():
    measure("anthropic", name, anthropic, lambda t, a=a: anthropic_messages(t, *a),
            lambda t, b=b: anthropic_messages(t, *b))
