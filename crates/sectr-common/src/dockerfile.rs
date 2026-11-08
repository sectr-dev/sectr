//! The Dockerfile lines that put an app in a runner image, shared by the
//! server's managed builds and the endpoint images `sectr init --endpoint`
//! writes, so the two build an app the same way.

/// The Lambda Web Adapter: turns Lambda invocations into HTTP requests to
/// the sidecar on port 8080. Pinned; AWS publishes it on ECR Public.
pub const LAMBDA_ADAPTER: &str = "public.ecr.aws/awsguru/aws-lambda-adapter:1.1.0";

/// Copy the app from `source` (in the build context) to `/app`, bake its
/// dependencies when it ships a `pyproject.toml`, and make it readable by
/// any user (runtimes may run the image as a non-root user).
///
/// `setup` is inserted as-is after `WORKDIR` (whole lines), and
/// `before_sync` runs in the dependency step just before `uv sync`; the
/// server uses them for the repository's own SDK path. Both may be empty.
pub fn app_build_steps(source: &str, setup: &str, before_sync: &str) -> String {
  format!(
    "COPY {source} /app\nWORKDIR /app\n{setup}\
     RUN if [ -f pyproject.toml ]; then {before_sync}uv sync --frozen; fi\n\
     RUN chmod -R a+rX /app\n"
  )
}

/// The Dockerfile of an endpoint image: the app on `base_image`, the sidecar
/// in serve mode as its command, dialing only `ingest_url`. With `lambda`,
/// it also carries the Lambda Web Adapter.
pub fn endpoint_dockerfile(base_image: &str, ingest_url: &str, lambda: bool) -> String {
  let mut file = String::from(
    "# The Sectr sidecar in serve mode, with this app: each turn, Sectr wakes\n\
     # it over HTTP and it dials back to run the turn. Written by\n\
     # `sectr init --endpoint`; rerun it with --force to update the pins.\n",
  );
  file.push_str(&format!("FROM {base_image}\n"));
  if lambda {
    file.push_str(&format!(
      "# Turns Lambda invocations into HTTP requests to the sidecar.\n\
       COPY --from={LAMBDA_ADAPTER} /lambda-adapter /opt/extensions/lambda-adapter\n"
    ));
  }
  file.push_str(&app_build_steps(".", "", ""));
  file.push_str(&format!(
    "# The only server the sidecar dials.\n\
     ENV SECTR_INGEST_URL={ingest_url}\n\
     # uv needs a writable home, and some platforms only allow writing to /tmp.\n\
     ENV HOME=/tmp\n\
     CMD [\"/usr/local/bin/sectr-worker\", \"--serve\"]\n"
  ));
  file
}

#[cfg(test)]
mod tests {
  use super::*;

  const BASE: &str = "ghcr.io/sectr-dev/sectr-runner-base:v1.2.3";
  const INGEST: &str = "wss://ingest.sectr.dev/ingest";

  #[test]
  fn the_lambda_file_adds_only_the_adapter() {
    let lambda = endpoint_dockerfile(BASE, INGEST, true);
    let other = endpoint_dockerfile(BASE, INGEST, false);
    let golden_other = "\
# The Sectr sidecar in serve mode, with this app: each turn, Sectr wakes
# it over HTTP and it dials back to run the turn. Written by
# `sectr init --endpoint`; rerun it with --force to update the pins.
FROM ghcr.io/sectr-dev/sectr-runner-base:v1.2.3
COPY . /app
WORKDIR /app
RUN if [ -f pyproject.toml ]; then uv sync --frozen; fi
RUN chmod -R a+rX /app
# The only server the sidecar dials.
ENV SECTR_INGEST_URL=wss://ingest.sectr.dev/ingest
# uv needs a writable home, and some platforms only allow writing to /tmp.
ENV HOME=/tmp
CMD [\"/usr/local/bin/sectr-worker\", \"--serve\"]
";
    assert_eq!(other, golden_other);
    let adapter = "# Turns Lambda invocations into HTTP requests to the sidecar.\n\
                   COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:1.1.0 \
                   /lambda-adapter /opt/extensions/lambda-adapter\n";
    assert_eq!(
      lambda,
      golden_other.replace("COPY . /app\n", &format!("{adapter}COPY . /app\n"))
    );
  }
}
