# Sectr

Sectr stores, streams and resumes AI agent conversations. It's the missing
database and coordinator between an AI agent's frontend and its compute: you
deploy an agent with `sectr deploy` and get durable sessions, SSE streaming and
human-in-the-loop approvals.

- **Docs:** [docs.sectr.dev](https://docs.sectr.dev)
- **Hosted:** [sectr.dev](https://sectr.dev)

## Repo layout

```
crates/         Rust workspace: sectr-common, sectr-server, sectr-worker, sectr-cli, sectr-conformance
schema/         Cross-component contracts (JSON Schema, OpenAPI, SQL)
sdk-python/     The `sectr` Python SDK
js/             npm packages, including @sectr/copilotkit
dashboard/      Next.js console: deployments, session inspector, approvals
docs-site/      docs.sectr.dev
examples/       Sample agents, which double as conformance fixtures
docker/         Server and runner base images
scripts/        Release and CI logic, driven by the justfile
```

To understand the system, start with
[`schema/README.md`](./schema/README.md). [`RELEASING.md`](./RELEASING.md) covers
checks and releases.

## Contributing

Issues are welcome, but outside pull requests aren't being accepted for now. See
[`CONTRIBUTING.md`](./CONTRIBUTING.md). To report a vulnerability, see
[`SECURITY.md`](./SECURITY.md).

## License

The platform is licensed under [Apache-2.0](./LICENSE). `sdk-python/`, `js/`,
`examples/` and `crates/sectr-cli/templates/` are licensed under MIT, so you can
copy them into your own projects. `schema/vendor/ag-ui/` keeps its upstream MIT
license.

The Sectr name and logo aren't licensed. If you distribute or host a modified
copy, please give it a different name.
