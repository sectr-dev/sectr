---
title: Run it on your own compute
description: Run your agent's code on your own AWS Lambda, Google Cloud Run or container platform, while Sectr keeps its sessions, journal and approvals.
---

By default, `sectr deploy` ships your agent to Sectr Run, which builds it and runs each turn in a sandbox. You can run it on your own compute instead: AWS Lambda, Google Cloud Run, or any container platform that serves HTTPS. Your code, your model keys and your tool calls then stay in your own cloud account.

Sectr still keeps the sessions, the journal and approvals, and serves the AG-UI endpoint. The dashboard works the same, and shows where each app runs.

## How it works

Your image is your agent plus the Sectr sidecar, built from Sectr's runner base image. For each turn:

1. Sectr sends a request to your endpoint's URL, called a **wake**.
2. The sidecar in your image dials back to Sectr over an outbound WebSocket and picks up the turn.
3. It runs your agent and streams the turn's events back to Sectr.
4. When the turn ends, the sidecar answers the wake.

The wake request stays open for the whole turn, because serverless platforms only run an instance while a request is open. Each instance runs one turn at a time.

Your endpoint holds no Sectr credentials. Each wake carries a one-time token, and the sidecar dials only the server named in its image. So your endpoint needs:

- **Inbound:** a public HTTPS URL that anyone can call.
- **Outbound:** access to Sectr's ingest, `wss://ingest.sectr.dev`.

## Set up a project

Log in first (`sectr auth login`): the Dockerfile pins your server's version and ingest URL, so `init` reads them from the server. Then start a project for your platform:

```sh
sectr init my-agent --endpoint lambda       # or cloud-run, or container
```

To move an existing project, run `sectr init --endpoint lambda` (or `cloud-run`, or `container`) in its directory. Either way you get:

- a `Dockerfile` and a `.dockerignore`, which keeps `.env` out of the image;
- an empty `endpoint` under `[deploy]` in `sectr.toml`, for your endpoint's URL;
- the steps for your platform, which the sections below give in full.

The generated `README.md` lists the same steps.

## AWS Lambda

The Dockerfile includes the [Lambda Web Adapter](https://github.com/awslabs/aws-lambda-web-adapter), which turns Lambda invocations into HTTP requests to the sidecar.

Run these from the project directory, with your own account id. Pick a region near your Sectr server: hosted Sectr runs in northern Virginia, so `us-east-1`.

```sh
APP=my-agent; REGION=us-east-1; ACCT=123456789012
REG=${ACCT}.dkr.ecr.${REGION}.amazonaws.com

# 1. Build the image
docker build --platform linux/amd64 --provenance=false -t $APP .

# 2. Push it to Amazon ECR
aws ecr create-repository --repository-name $APP --region $REGION
aws ecr get-login-password --region $REGION | docker login --username AWS --password-stdin $REG
docker tag $APP $REG/$APP:latest
docker push $REG/$APP:latest

# 3. An execution role that can only write logs
aws iam create-role --role-name ${APP}-exec --assume-role-policy-document \
  '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"lambda.amazonaws.com"},"Action":"sts:AssumeRole"}]}'
aws iam attach-role-policy --role-name ${APP}-exec \
  --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole

# 4. The function. If it says the role can't be assumed yet, wait a few seconds and run it again.
aws lambda create-function --function-name $APP --package-type Image \
  --code ImageUri=$REG/$APP:latest --role arn:aws:iam::${ACCT}:role/${APP}-exec \
  --timeout 900 --memory-size 1024 --architectures x86_64 --region $REGION
aws lambda wait function-active-v2 --function-name $APP --region $REGION

# 5. A function URL, and the two permissions a public URL needs
aws lambda create-function-url-config --function-name $APP --auth-type NONE --region $REGION
aws lambda add-permission --function-name $APP --statement-id url-public --region $REGION \
  --action lambda:InvokeFunctionUrl --principal '*' --function-url-auth-type NONE
aws lambda add-permission --function-name $APP --statement-id url-invoke --region $REGION \
  --action lambda:InvokeFunction --principal '*' --invoked-via-function-url
```

`create-function-url-config` prints the `FunctionUrl`. Put it in `sectr.toml`, then [register it](#register-it).

Write `${ACCT}` in braces where a `:` follows it, as in the role's ARN. zsh, the default shell on macOS, reads `$ACCT:r` as a modifier and drops the `:r`.

- **Timeout:** 900 seconds is Lambda's maximum, and a turn can't run longer than the function's timeout. See [time limits](#time-limits).
- **Memory:** 1024 MB or more. Lambda gives CPU in proportion to memory, and the agent's start counts toward the time Sectr waits for each turn to start.
- **Permissions:** the second `add-permission` lets anyone invoke the function, but only through its function URL.
- **Arm:** to run on Graviton, build with `--platform linux/arm64` and create the function with `--architectures arm64`.
- **In a VPC:** the function needs a NAT gateway to reach Sectr's ingest.

To ship a new version, build and push the image again, then:

```sh
aws lambda update-function-code --function-name $APP --image-uri $REG/$APP:latest --region $REGION
aws lambda wait function-updated-v2 --function-name $APP --region $REGION
sectr deploy
```

## Google Cloud Run

From the project directory, with your own project id and a region near your Sectr server (for hosted Sectr, `us-east4`):

```sh
gcloud run deploy my-agent --source . --region us-east4 --project PROJECT_ID \
  --allow-unauthenticated --timeout 3600 --concurrency 1
```

Cloud Build builds the image from the Dockerfile. The command prints the service URL. Put it in `sectr.toml`, then [register it](#register-it).

- **`--allow-unauthenticated`:** Sectr calls your service without Google credentials. The one-time token in each wake is what lets a turn run.
- **`--timeout 3600`:** a turn can't run longer than the request timeout, and 3600 seconds is Cloud Run's maximum. See [time limits](#time-limits).
- **`--concurrency 1`:** each instance runs one turn at a time, so each concurrent turn gets its own instance.
- **CPU:** the default, CPU only while a request is open, works. The wake request is open for the whole turn.
- **Instances:** `--max-instances` caps how many turns run at once. Cold starts count toward the time Sectr waits for each turn to start, so `--min-instances 1` helps if yours are slow.

To ship a new version, run the same `gcloud run deploy`, which keeps the URL, then `sectr deploy`.

## Your own container platform

Any platform that runs a container behind an HTTPS URL works, for example ECS behind a load balancer, or Kubernetes behind an ingress:

```sh
docker build --platform linux/amd64 -t my-agent .     # linux/arm64 for Arm hosts
```

Run it with:

- **its port published**, and `PORT` set if the platform needs a port other than 8080;
- **a public HTTPS URL** in front;
- **requests allowed to last as long as a turn**, including on any proxy or load balancer in between;
- **one request at a time per container.** A container that's already running a turn answers a wake with `409`. Sectr then tries again, until the turn's start deadline.

Put the URL in `sectr.toml`, then [register it](#register-it).

## Register it

With the URL in `sectr.toml`:

```toml
[deploy]
endpoint = "https://abc123.lambda-url.us-east-1.on.aws/"
```

run:

```sh
sectr deploy
```

`sectr deploy` uploads nothing. It checks that your endpoint is a Sectr sidecar, then has it dial back to Sectr and import your agent inside your image. That checks the whole loop before any turn runs. Then it registers the URL as your app's new version and prints the app's dashboard link and AG-UI URL.

Run `sectr deploy` again whenever you ship a new image or change `sectr.toml`. Sectr reads your agent's tools and settings at registration.

## Secrets

Your agent can get its keys two ways:

- **Sectr's secrets store:** `sectr secrets set OPENAI_API_KEY`, and the name in your `AgentApp`'s `env` list, as on Sectr Run. Sectr hands the values to the agent at the start of each turn. See [Secrets](/guides/secrets/).
- **Your platform's own:** Lambda environment variables, Cloud Run's `--set-env-vars` or `--set-secrets`, or your platform's secret manager through an instance role. The agent sees the environment it runs in.

If both set the same name, Sectr's value wins.

## Time limits

| | Longest one invocation can run |
| -- | -- |
| AWS Lambda | The function's timeout: 15 minutes at most |
| Google Cloud Run | The request timeout: 60 minutes at most |
| Hosted Sectr | Holds a wake open for 60 minutes at most |

**Approval pauses split a turn.** When a tool needs approval, the invocation ends and the instance is freed. The approval starts a new wake, with its own time limit, so time spent waiting for a person never counts.

**At the limit**, the platform ends the invocation. Sectr ends the turn within a few seconds, with a session error that says why, for example:

```text
your compute endpoint stopped before the turn finished: the wake request ended with HTTP 502. If the turn ran as long as the function's timeout, AWS Lambda ended it there (15 minutes at most).
```

A tool call that was running shows as interrupted. The session itself goes on: send the next message as usual.

**Starting a turn:** your endpoint has 60 seconds from the wake to dial back, cold start included. Otherwise the turn fails with `endpoint wake failed: your compute endpoint did not start the turn within 60s`.

## Concurrency

Each running turn takes its own instance. Hosted Sectr runs up to 20 turns of one app at once. Past that, a new turn gets a `429` with `Retry-After`, and `sectr send` waits and retries.

Your platform has its own limits: Lambda's concurrency quota for your account (low on new accounts, and you can raise it), or Cloud Run's `--max-instances`. When the platform turns a wake away for capacity, Sectr tries again until the turn's start deadline.

## Upgrading

The Dockerfile's `FROM` line pins the runner base image to your server's version. When the server is newer, `sectr deploy` warns that the sidecar's version differs. To update:

1. Change the version in the `FROM` line, or run `sectr init --endpoint <platform> --force`, which rewrites the `Dockerfile` and `.dockerignore` from scratch.
2. Rebuild and redeploy the image.
3. Run `sectr deploy`.

## When something fails

`sectr deploy` adds a hint to most of these.

| The error says | What to check |
| -- | -- |
| `answered HTTP 403` | Lambda: the function URL's auth type is `NONE`, and both permissions from step 5 are there. Cloud Run: the service allows unauthenticated invocations |
| `could not connect to`, or `didn't answer in time` | The URL in `sectr.toml`, and that the service is deployed and running |
| `resolves to a private address` | The endpoint must be reachable on the public internet |
| `dialed a server that doesn't know this` | `SECTR_INGEST_URL` in the Dockerfile names another server. Fix it or rerun `sectr init --endpoint <platform> --force`, then rebuild |
| `couldn't reach` | Your platform blocks outbound connections. A Lambda function in a VPC needs a NAT gateway |
| `the manifest import failed in your image` | Your agent failed to import. The error goes on with its output |
| `doesn't accept endpoint deployments` | The server has this off. On a [self-hosted](/guides/self-hosting/) server, its admin turns it on with `SECTR_ENDPOINT_INGEST_URL` |
| `did not start the turn within` | A slow cold start, or an instance that couldn't reach Sectr's ingest. Check the platform's logs |

On a self-hosted server, the wake deadline, the turn timeout and the concurrency caps are settings: see [Self-hosting](/guides/self-hosting/#settings).
