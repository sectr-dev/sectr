# The sectr runner BASE image — what every deploy's image builds on:
# python + uv + the sectr SDK + the sidecar binary (`sectr-worker`).
#
# Build ON the deploy host (the server + the daemon run on one
# box); the multi-stage build compiles
# the Rust side inside the image so the binaries' glibc matches the
# runtime stage exactly — no cross-building, works on any host.
#
#   docker build -f docker/runner-base.Dockerfile -t sectr-runner-base:latest .
#
# The resulting image is what `SECTR_RUNNER_BASE_IMAGE` names.
#
# syntax=docker/dockerfile:1
FROM rust:slim-bookworm AS bins
WORKDIR /src
COPY Cargo.toml Cargo.lock /src/
COPY crates /src/crates
RUN cargo build --release -p sectr-worker --bins

FROM python:3.12-slim-bookworm
# uv for per-deploy dependency baking (build step: `uv sync`; apps without
# a pyproject.toml use the SDK pip-installed below directly).
RUN pip install --no-cache-dir uv
COPY --from=bins /src/target/release/sectr-worker /usr/local/bin/sectr-worker
# The SDK (FastAPI runner-contract adapter included — base dep).
COPY sdk-python /opt/sdk-python
RUN pip install --no-cache-dir /opt/sdk-python
# Readable + traversable by ANY user: runtimes may exec as non-root (E2B
# sandboxes run envd processes as uid-1000 `user`, live-verified
# 2026-09-17 — the agent's editable SDK import died on EACCES against
# root-owned 644 files). Cloud Run/docker run as root and never notice,
# so this is a portability fix, not a regression fix.
RUN chmod -R a+rX /opt/sdk-python
# The user code mounts at /app (the build step COPYs it; the runner's
# workdir is /app).
WORKDIR /app
