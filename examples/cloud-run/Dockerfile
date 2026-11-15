# The Sectr sidecar in serve mode, with this app: each turn, Sectr wakes
# it over HTTP and it dials back to run the turn. Written by
# `sectr init --endpoint`; rerun it with --force to update the pins.
FROM ghcr.io/sectr-dev/sectr-runner-base:v0.2.3
COPY . /app
WORKDIR /app
RUN if [ -f pyproject.toml ]; then uv sync --frozen; fi
RUN chmod -R a+rX /app
# The only server the sidecar dials.
ENV SECTR_INGEST_URL=wss://ingest.sectr.dev/ingest
# uv needs a writable home, and some platforms only allow writing to /tmp.
ENV HOME=/tmp
CMD ["/usr/local/bin/sectr-worker", "--serve"]
