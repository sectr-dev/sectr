# LangGraph support agent on Sectr

The support-agent scenario (look up an order, refund it behind an approval)
as a LangGraph `StateGraph` with **no checkpointer** — the Sectr journal is
the checkpoint.

This example is the adapter's CI fixture (`sdk-python/tests/test_example_langgraph.py`
pins its golden journal). The docs sample lives in
[`sectr-copilotkit-starter`](https://github.com/sectr-dev/sectr-copilotkit-starter)
(`agent/support_graph.py`, the `/chat_langgraph` route), with a chat UI in
front of it; see [Using LangGraph](https://docs.sectr.dev/getting-started/langgraph/).

| File | What it is |
|---|---|
| `tools.py` | Framework-free tools (`sectr.tools.tool`); `refund` has `needs_approval=True`. |
| `main.py` | The graph (agent node + `ToolNode`), tools converted with `as_langgraph_tool`, one adapter line: `app.add_route("/chat", langgraph(graph))`. |
| `fake.py` | Scripted chat model for offline runs (`SECTR_FAKE=1`). |

## How a turn maps

- Every turn is a fresh `graph.astream`; its messages are rebuilt from the
  journaled transcript. Nothing of LangGraph's state survives between turns.
- A gated tool (`refund`) raises `ApprovalRequired` inside the tool node. The
  adapter journals the model's tool call and ends the turn with
  `TOOL_APPROVAL_REQUIRED` — nothing executed yet.
- On approval the adapter runs the refund exactly once (a denial journals the
  standard rejection text instead), journals the `TOOL_RESULT`, and re-runs
  the graph on the answered conversation. The model only writes the
  continuation.

Gate tools with `as_langgraph_tool`, not LangGraph's `interrupt()`: a native
`interrupt()` needs a checkpointer, so under Sectr it fails loudly (and
`sectr inspect` warns when it finds the import). Supported graphs are
ReAct-shaped — one `ToolNode` holding your sectr tools. Full details:
[LangGraph adapter reference](../../docs-site/src/content/docs/reference/sdk/adapters.md).

## Run it

Offline, no key (scripted model):

```sh
# terminal 1 — long-running dev engine + gateway
cd examples/langgraph && SECTR_FAKE=1 sectr dev

# terminal 2 — same directory
sectr send "where is order 123?"                 # lookup turn; prints the session id
sectr send --session <id> "please refund it"     # suspends: awaiting approval
sectr send --session <id> --approve              # refund runs once, then the continuation
```

Real model (OpenRouter): put `OPENROUTER_API_KEY` in `.env` (optionally
`SECTR_MODEL`, default `z-ai/glm-5.3-flash`) and run `sectr dev` without
`SECTR_FAKE`. Deployed: `sectr deploy --path examples/langgraph` and set the
`OPENROUTER_API_KEY` secret (`sectr secrets set`).

## Verify

`sdk-python/tests/test_example_langgraph.py` drives this exact module through
the runner server and pins the golden journal sequence for the whole scenario
(approve and deny paths).
