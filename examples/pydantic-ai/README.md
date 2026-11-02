# Pydantic AI support agent on Sectr

The support-agent scenario (look up an order, refund it behind an approval)
as a plain `pydantic_ai.Agent`. `refund` is a native deferred tool
(`requires_approval=True`); Pydantic AI resumes the paused run from the
message history alone, which is exactly Sectr's "transcript in, events out"
model.

This example is the adapter's CI fixture
(`sdk-python/tests/test_example_pydantic_ai.py` pins its golden journal).

| File | What it is |
|---|---|
| `tools.py` | Plain functions, no framework or sectr imports. |
| `main.py` | The `Agent` (tools registered, `refund` marked `requires_approval=True`) and one adapter line: `app.add_route("/chat", pydantic_ai(agent))`. |
| `fake.py` | Scripted, strict `FunctionModel` for offline runs (`SECTR_FAKE=1`). |

## How a turn maps

- Every turn is a fresh `agent.iter(...)` on a `message_history` rebuilt from
  the journaled transcript. Nothing of Pydantic AI's state survives between
  turns.
- A call to `refund` ends the run with `DeferredToolRequests`. The adapter
  journals the model's tool call and ends the turn with
  `TOOL_APPROVAL_REQUIRED`; nothing executed yet.
- On approval, Pydantic AI itself runs the refund exactly once (validation,
  `RunContext`, `deps` and hooks all apply); a denial journals the standard
  rejection text instead. The model only writes the continuation.
- Several gated calls in one model response become sequential approvals, one
  per turn, without re-running anything.

Full details: [Pydantic AI adapter reference](../../docs-site/src/content/docs/reference/sdk/adapters.md).

## Run it

Offline, no key (scripted model):

```sh
# terminal 1 — long-running dev engine + gateway
cd examples/pydantic-ai && SECTR_FAKE=1 sectr dev

# terminal 2 — same directory
sectr send "where is order 123?"                 # lookup turn; prints the session id
sectr send --session <id> "please refund it"     # suspends: awaiting approval
sectr send --session <id> --approve              # refund runs once, then the continuation
```

Real model (OpenRouter): put `OPENROUTER_API_KEY` in `.env` (optionally
`SECTR_MODEL`, default `z-ai/glm-5.3-flash`) and run `sectr dev` without
`SECTR_FAKE`. Deployed: `sectr deploy --path examples/pydantic-ai` and set the
`OPENROUTER_API_KEY` secret (`sectr secrets set`).

## Verify

`sdk-python/tests/test_example_pydantic_ai.py` drives this exact module through
the runner server and pins the golden journal sequence for the whole scenario
(approve and deny paths).
