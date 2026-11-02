---
title: SDK reference
description: The sectr Python SDK — modules, mental model, install.
---

<!-- skill: skip file — a pointer page; SKILL.md links each page it points to -->

The `sectr` SDK ([PyPI: `sectr`](https://pypi.org/project/sectr/)) is deliberately small - it only needs to translate the events of your agent into Sectr's internal event format, and handle resumption of your agent.

Install it in your agent project with:

```sh
uv add sectr
```

See [your first agent](/getting-started/first-agent/) for a guided walkthrough of building an agent, the primary class of [AgentApp](/reference/sdk/app/), or the available [framework adapters](/reference/sdk/adapters/).
