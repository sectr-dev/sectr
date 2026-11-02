"""AgentApp — route registry and manifest source.

Handlers all share one signature — `(SessionContext) -> AsyncIterator[
RunnerEvent]` — whether hand-written (`@app.agent_route`) or produced by a
framework adapter (`app.add_route("/chat", openai_agents(agent))`). The
FastAPI adapter (`sectr.server`) mounts the registry as `POST /invocations`
endpoints dispatched by `route`; `sectr.inspect` extracts the manifest.
"""

from __future__ import annotations

import re
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass
from typing import Any

from .context import SessionContext
from .events import RunnerEvent

__all__ = ["AgentApp", "Route", "Handler"]

Handler = Callable[[SessionContext], AsyncIterator[RunnerEvent]]

_ROUTE_RE = re.compile(r"^/[A-Za-z0-9_\-/]*$")


@dataclass
class Route:
    path: str
    handler: Handler


class AgentApp:
    """Ordered route registry. One per user project; discovered by
    `sectr inspect` via the `entry = "module:attribute"` in sectr.toml."""

    def __init__(
        self,
        *,
        name: str | None = None,
        env: list[str] | None = None,
    ) -> None:
        self.name = name
        self.routes: list[Route] = []
        # Env var NAMES the sidecar must inject from the secret store at
        # spawn. Values never pass through the manifest (write-only secrets).
        self.env: tuple[str, ...] = tuple(env or ())

    def add_route(self, path: str, handler: Handler) -> None:
        """Register `handler` at `path` (e.g. "/customer-support"). Paths
        must start with "/" and be unique — the platform routes sessions by
        (app, path)."""
        if not _ROUTE_RE.match(path):
            raise ValueError(f"invalid route path {path!r} — must start with '/'")
        if any(r.path == path for r in self.routes):
            raise ValueError(f"route {path!r} already registered")
        self.routes.append(Route(path=path, handler=handler))

    def agent_route(self, path: str) -> Callable[[Handler], Handler]:
        """Decorator flavor for hand-written handlers:

            @app.agent_route("/chat")
            async def chat(ctx: SessionContext): ...
        """

        def register(handler: Handler) -> Handler:
            self.add_route(path, handler)
            return handler

        return register

    def manifest(self) -> dict[str, Any]:
        """The quick manifest (`sectr inspect` scan mode). The AUTHORITATIVE
        manifest is extracted by importing this app under SECTR_MANIFEST=1 —
        see `sectr.inspect`."""
        return {
            "name": self.name,
            "routes": [{"path": r.path} for r in self.routes],
        }
