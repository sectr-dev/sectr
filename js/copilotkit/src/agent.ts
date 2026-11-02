// SectrAgent: an AG-UI HttpAgent for a Sectr route that can also reconnect.
import {
  HttpAgent,
  runHttpRequest,
  transformHttpEventStream,
  type BaseEvent,
  type RunAgentInput,
} from "@ag-ui/client";
import { concat, defer, EMPTY, Observable, throwError } from "rxjs";

// `HttpAgent` plus AG-UI's `connect()`: a reloaded page reattaches to its
// conversation through Sectr's read-only endpoint (`GET <route>?threadId=`).
// It gets the thread's history plus its latest run replayed: still streaming
// if the agent is mid-turn, or ending in the approval that's waiting for a
// decision. A new thread has no run, so the stream is empty. (`HttpAgent`
// leaves `connect()` unimplemented, so a reloaded page used to start empty.)
//
// Use it wherever you'd use an `HttpAgent` for a Sectr route: in the browser
// for direct mode (`selfManagedAgents`), or in the Copilot Runtime's agents
// map, where `SectrRunner` reconnects through it.
export class SectrAgent extends HttpAgent {
  protected connect(input: RunAgentInput): Observable<BaseEvent> {
    return this.connectThread(input.threadId);
  }

  // The reconnect stream for `threadId`, with this agent's headers plus
  // `headers` (`SectrRunner` passes the browser's forwarded `Authorization`).
  connectThread(threadId: string, headers: Record<string, string> = {}): Observable<BaseEvent> {
    return defer(() => {
      const url = new URL(this.url);
      url.searchParams.set("threadId", threadId);
      const merged = new Headers(this.headers);
      for (const [name, value] of Object.entries(headers)) merged.set(name, value);
      merged.set("Accept", "text/event-stream");
      let broken: { error: unknown } | undefined;
      const stream = transformHttpEventStream(
        runHttpRequest(async () =>
          quietBody(
            await this.fetch(url.toString(), { method: "GET", headers: merged }),
            (error) => (broken = { error }),
          ),
        ),
      );
      return concat(stream, defer(() => (broken ? throwError(() => broken!.error) : EMPTY)));
    });
  }
}

// @ag-ui/client (1.0.1) cancels the response body when the stream ends and
// rethrows anything but an AbortError from that cancel. Cancelling a fetch
// body that broke, or that is cancelled mid-read (a page reloading mid-turn),
// rejects with undici's "terminated", so that escapes as an unhandled
// rejection, which crashes a plain Node server. So it gets a body that never
// errors and whose cancel never rejects: a broken stream just ends, and
// `onError` lets the caller raise the error after it.
function quietBody(response: Response, onError: (error: unknown) => void): Response {
  if (!response.ok || !response.body) return response;
  const reader = response.body.getReader();
  const body = new ReadableStream<Uint8Array>({
    async pull(controller) {
      try {
        const { done, value } = await reader.read();
        if (done) controller.close();
        else controller.enqueue(value);
      } catch (error) {
        onError(error);
        try {
          controller.close();
        } catch {
          // Already cancelled: nobody is reading.
        }
      }
    },
    cancel: (reason) => reader.cancel(reason).catch(() => {}),
  });
  return new Response(body, {
    status: response.status,
    statusText: response.statusText,
    headers: response.headers,
  });
}
