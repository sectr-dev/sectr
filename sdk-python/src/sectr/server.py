"""FastAPI adapter: mounts an `AgentApp` as the runner invoke contract
(`schema/invocation.schema.json`).

- `create_asgi(app)` builds a FastAPI app with `POST /invocations` that:
    1. structurally validates the request body (full JSON-Schema validation
       is golden-tested in `schema/`; TODO(phase-2): validate with
       jsonschema at runtime too),
    2. dispatches by `route` to the registered handler,
    3. builds a `SessionContext` (transcript envelopes, input,
       the polymorphic reason),
    4. streams the handler through `sectr.drive` (finality + NO_MORE_ACTIONS rules)
       as SSE frames — `data:` = one runner event JSON per frame.
- `run(app)` reads `$SECTR_PORT` and serves on localhost (the sidecar is
  the only expected caller); runnable standalone via uvicorn for poking.
"""

from __future__ import annotations

import json
import os
import sys
from collections.abc import AsyncIterator
from typing import Any

from fastapi import FastAPI, HTTPException, Request
from fastapi.responses import StreamingResponse

from .app import AgentApp
from .context import SessionContext
from .drive import drive
from .events import Envelope

__all__ = ["create_asgi", "run"]

_SSE_MEDIA_TYPE = "text/event-stream"


def _validate_request(body: Any) -> None:
    """Structural checks mirroring invocation.schema.json (required keys and
    top-level types). JSON-Schema validation TODO(phase-2)."""
    if not isinstance(body, dict):
        raise HTTPException(status_code=422, detail="request body must be an object")
    required: dict[str, type | tuple[type, ...]] = {
        "session_id": str,
        "invocation_id": str,
        "route": str,
        "transcript": list,
        "reason": dict,
    }
    for key, expected in required.items():
        if key not in body:
            raise HTTPException(status_code=422, detail=f"missing field {key!r}")
        if not isinstance(body[key], expected):
            raise HTTPException(
                status_code=422, detail=f"field {key!r} has the wrong type"
            )
    if not body["route"].startswith("/"):
        raise HTTPException(status_code=422, detail="'route' must start with '/'")
    reason = body["reason"]
    if reason.get("kind") == "user_message":
        text = reason.get("input")
        if not isinstance(text, str) or not text.strip():
            raise HTTPException(
                status_code=422,
                detail="a user_message reason needs a non-blank string 'input': the user's message text",
            )
    for frame in body["transcript"]:
        if not isinstance(frame, dict):
            raise HTTPException(
                status_code=422, detail="transcript must contain event objects"
            )


async def _sse(frames: AsyncIterator[dict[str, Any]]) -> AsyncIterator[bytes]:
    async for frame in frames:
        yield f"data: {json.dumps(frame)}\n\n".encode()


def create_asgi(app: AgentApp) -> FastAPI:
    """Build the FastAPI ASGI app implementing POST /invocations."""
    fastapi_app = FastAPI(title="sectr-runner")
    handlers = {route.path: route.handler for route in app.routes}

    @fastapi_app.post("/invocations")
    async def invocations(request: Request) -> StreamingResponse:
        body = await request.json()
        _validate_request(body)
        handler = handlers.get(body["route"])
        if handler is None:
            raise HTTPException(
                status_code=404, detail=f"unknown route {body['route']!r}"
            )
        ctx = SessionContext(
            transcript=[Envelope.from_dict(e) for e in body["transcript"]],
            reason=body["reason"],
            session_id=body["session_id"],
            invocation_id=body["invocation_id"],
        )
        return StreamingResponse(
            _sse(drive(handler, ctx)),
            media_type=_SSE_MEDIA_TYPE,
        )

    @fastapi_app.get("/healthz")
    async def healthz() -> dict[str, str]:
        return {"status": "ok"}

    return fastapi_app


def run(app: AgentApp) -> None:
    """Serve on localhost:$SECTR_PORT (default 8080 in dev). The sidecar is
    the only expected caller in production; standalone uvicorn is for manual
    poking."""
    import uvicorn

    port = int(os.environ.get("SECTR_PORT", "8080"))
    uvicorn.run(create_asgi(app), host="127.0.0.1", port=port, log_level="info")


def main(argv: list[str] | None = None) -> int:
    """`python -m sectr.server <module:attribute>` — resolve the AgentApp and
    serve the invoke contract on localhost:$SECTR_PORT. This is the entry the
    dev engine spawns (`sectr dev` runs `uv run python -m sectr.server
    <entry>`); the sidecar uses `run(app)` directly in production."""
    argv = sys.argv[1:] if argv is None else argv
    if not argv:
        print("usage: python -m sectr.server <module:attribute>", file=sys.stderr)
        return 2
    from .inspect import _resolve_app

    run(_resolve_app(argv[0]))
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
