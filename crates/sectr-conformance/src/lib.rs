//! Adapter conformance harness.
//!
//! A [`Scenario`](scenario::Scenario) pairs an agent definition with cases.
//! The agent ([`agent::AgentSpec`]) is data: the compiler turns it into one
//! sectr project per [`adapter::Adapter`]. Cases are ordinary async Rust
//! against a [`case::Case`], which stands in for the model (it answers the
//! agent's model requests), the tools (it decides each execution's outcome)
//! and the journal (it injects invocations and checks what the adapter
//! writes back).
//!
//! Every case runs once per adapter and [`endpoint::Endpoint`] (the model
//! API's shape), as `adapter/endpoint/scenario/case`. The case code and the
//! journal it expects are the same on every endpoint: the mock parses each
//! endpoint's requests into one history and encodes one reply.
//!
//! # Ordering and strictness
//!
//! What a case pins exactly, and what it leaves to the adapter or the SDK:
//!
//! | Side | Exact | Loose |
//! | -- | -- | -- |
//! | Journal ([`Case::journal`](case::Case::journal)) | Event order, from a cursor, nothing skipped. `TOOL_RESULT`s in **completion** order (which the case decides, below). Call ids verbatim. | Text deltas compared concatenated per message; message ids checked for consistency, not value. [`ev::one_of`] where the framework picks the next event (e.g. which gated call pauses first); [`Case::paused`](case::Case::paused) says which. |
//! | Model requests ([`ModelCall::expect_history`](mock::ModelCall::expect_history)) | The whole conversation, in order. Tool results in **call** order, whatever order they completed in. | The system prompt is pulled out and only checked to contain the agent's instructions; every request must offer the agent's tools, and may offer more. |
//! | Tool executions ([`Case::tools`](case::Case::tools)) | Name and arguments. Each runs only when the case answers it. | **Arrival** order: each execution matches its answer wherever it is listed. |
//!
//! The list order of [`Case::tools`](case::Case::tools) is the completion
//! order: executions in flight together are answered in that order, 200ms
//! apart. One that only starts after another finishes is answered when it
//! arrives.
//!
//! Strictness: every model request and tool execution must be answered by
//! the case. Unmatched journal output, an unanswered request, or a turn
//! still running fails at the next `user` / `approve` / `deny`, or when the
//! case returns. A journal wait fails within 2s, naming the request, when
//! the agent is blocked on one the case hasn't answered; any other wait
//! fails at once if the turn has ended, otherwise after 30s. Events the
//! case caused (`SESSION_CREATED`, `INVOCATION_STARTED`, approval
//! decisions, a [`seed`]ed prefix) are inputs and never matched.
//!
//! One exception to answering in lockstep:
//! [`Case::answer_ahead`](case::Case::answer_ahead) answers executions on
//! arrival (tools that finish at once). Each answer is used once: a repeat
//! still reaches the case as unexpected, and an unused answer fails the
//! case at the end.
//!
//! Both modes ([`scenario::both_modes`]): a scenario's gated twin
//! (`<id>_gated`) runs the same case code with every tool gated. The
//! harness approves each pause itself and drops its `TOOL_APPROVAL_REQUIRED`
//! from the journal output, so every exact check above must hold unchanged:
//! the approval-resume path must journal and send the model exactly what the
//! framework's own path does. Every execution must also follow its own
//! approval.
//!
//! Gates ([`scenario::Gate`]): a gated tool is gated each framework's usual
//! way. langgraph has other ways to gate a tool in code
//! (`HumanInTheLoopMiddleware`, `as_langgraph_tool(t, needs_approval=True)`
//! on a plain LangChain tool), so the scenarios that approve or deny run
//! again through each, on langgraph only (`<id>_middleware`,
//! `<id>_wrapped`), with the same case code.

pub mod adapter;
pub mod agent;
pub mod case;
pub mod compile;
pub mod endpoint;
pub mod ev;
pub mod harness;
pub mod mock;
pub mod msg;
pub mod replay;
pub mod report;
pub mod scenario;
pub mod scenarios;
pub mod seed;
pub mod warning;
