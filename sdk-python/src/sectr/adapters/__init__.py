"""Framework adapters (optional extras). Each adapter is a handler factory:
it takes the framework's native object and returns a
`(SessionContext) -> AsyncIterator[RunnerEvent]` handler, so registration is
one line per app:

    app.add_route("/chat", openai_agents(agent))
    app.add_route("/chat", langgraph(graph))
    app.add_route("/chat", pydantic_ai(agent))
    app.add_route("/chat", claude_code(options))

Mapping details (pause/resume semantics, transcript reconstruction) are
documented per module and pinned by the variant sketches in `examples/`.
"""

# The error code every adapter fails a turn with on a gated tool inside a
# subagent (the beta subagent contract: subagents are opaque, and Sectr
# can't pause and resume inside one yet).
SUBAGENT_GATED_TOOL = "subagent_gated_tool"
