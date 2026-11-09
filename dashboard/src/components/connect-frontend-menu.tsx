"use client";

import { useState } from "react";
import Link from "next/link";
import { ChevronDown, LoaderCircle, TriangleAlert } from "lucide-react";
import { CopilotKitIcon } from "@/components/framework-icons";
import {
    CodeSnippet,
    CommandBlock,
    ModeTabs,
    OptionMenu,
    Segmented,
    STEPS_CONTENT_CLASS,
    STEPS_OVERLAY_CLASS,
    StepList,
    skillPrompt,
    type MenuOption,
    type Step,
} from "@/components/new-project-card";
import { DropdownMenu as DropdownMenuPrimitive } from "radix-ui";
import { InlineCommand } from "@/components/welcome";
import { Button } from "@/components/ui/button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { publicServerUrl } from "@/lib/runtime-config";
import type { App } from "@/lib/types";
import { aguiUrl } from "@/lib/view";

const DOCS = "https://docs.sectr.dev";

/** The frontends the header menu offers (CopilotKit only, for now). */
const FRONTENDS: MenuOption[] = [
    {
        icon: CopilotKitIcon,
        title: "CopilotKit via AG-UI",
        description: "Connect your CopilotKit frontend via AG-UI",
    },
];

/** The public server base the AG-UI URL hangs off, as the CLI would print
 *  it: the public API in prod and mock, the gateway under `sectr dev`.
 *  (Never API_URL in prod: that's the same-origin /api rewrite.) Also the
 *  base of the invoke API (`POST /sessions`). */
export function aguiBase(): string {
    if (api.MOCK_MODE) return "https://api.sectr.dev";
    return api.PROD_MODE ? publicServerUrl() : api.API_URL;
}

/** Dev gateway (`sectr dev`): no key is needed. */
const DEV_GATEWAY = !api.MOCK_MODE && !api.PROD_MODE;

/**
 * The project header's "Connect your frontend ▾": a frontend-type menu
 * (CopilotKit only for now) that opens the setup modal. Rendered by
 * AppPageHeader, so it's on every project tab.
 */
export function ConnectFrontendMenu({ app }: { app: App }) {
    const [open, setOpen] = useState(false);
    return (
        <>
            <OptionMenu
                trigger={
                    <Button size="sm">
                        Connect your frontend
                        <ChevronDown />
                    </Button>
                }
                options={FRONTENDS}
                onPick={() => setOpen(true)}
                footer={
                    <DropdownMenuPrimitive.Item asChild>
                        <a
                            href={`${DOCS}/reference/api/ag-ui/`}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="block rounded px-1.5 pb-1 text-xs text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:text-foreground data-highlighted:text-foreground"
                        >
                            Any AG-UI client works today. See docs →
                        </a>
                    </DropdownMenuPrimitive.Item>
                }
            />
            <Dialog open={open} onOpenChange={setOpen}>
                <DialogContent
                    overlayClassName={STEPS_OVERLAY_CLASS}
                    className={`${STEPS_CONTENT_CLASS} sm:max-w-2xl`}
                >
                    <DialogHeader>
                        <DialogTitle className="text-xl">
                            Connect CopilotKit
                        </DialogTitle>
                        <DialogDescription>
                            Connect your CopilotKit app to {app.name}.
                        </DialogDescription>
                    </DialogHeader>
                    <ConnectBody app={app} />
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5 text-sm text-muted-foreground">
                        Starting fresh?
                        <InlineCommand command="sectr init my-app --frontend copilotkit" />
                        scaffolds all of this.
                    </div>
                    <a
                        href={`${DOCS}/getting-started/connecting-frontend/`}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="text-xs text-muted-foreground underline-offset-2 transition-colors hover:text-foreground hover:underline"
                    >
                        Full guide →
                    </a>
                </DialogContent>
            </Dialog>
        </>
    );
}

/** The modal's body. Mounted only while the dialog is open, so a created
 *  key (shown once) and the route choice reset on close. */
function ConnectBody({ app }: { app: App }) {
    const routes = app.routes.filter((r) => r.path.replace(/^\/+/, ""));
    const [route, setRoute] = useState(routes[0]?.path ?? "");
    const [key, setKey] = useState<string | null>(null);
    const url = aguiUrl(app, route, aguiBase()) ?? "<your agent's AG-UI URL>";

    const picker = routes.length > 1 && (
        <Segmented
            label="Agent"
            value={route}
            options={routes.map((r) => ({ id: r.path, label: r.path }))}
            onChange={setRoute}
        />
    );
    const keyControl = (showKey: boolean) => (
        <KeyControl app={app} keyValue={key} onKey={setKey} showKey={showKey} />
    );

    const env = `SECTR_AGUI_URL=${url}\nSECTR_INVOCATION_KEY=${DEV_GATEWAY ? "" : (key ?? "  # create one below")}`;

    const steps: Step[] = [
        {
            title: "Install the Sectr package",
            command: "npm install @sectr/copilotkit",
            // note: "Works with the CopilotKit packages your app already has (1.77 or later).",
        },
        {
            title: "Add your environment",
            extra: (
                <div className="flex flex-col gap-2">
                    <CodeSnippet file=".env.local" code={env} lang="dotenv" />
                    {keyControl(false)}
                </div>
            ),
            note: "Server-side only. The key mints a token per signed-in user and should never reach the browser.",
        },
        {
            title: "Add the Copilot Runtime route",
            extra: (
                <CodeSnippet
                    file="app/api/copilotkit/[[...slug]]/route.ts"
                    code={RUNTIME_ROUTE}
                    lang="ts"
                />
            ),
            note: "You may already have a Copilot route in your app, in which case you only need to adjust it to use SectrAgent and SectrRunner.",
        },
        {
            title: "Create a route to mint a token per user",
            extra: (
                <CodeSnippet
                    file="app/api/agent-token/route.ts"
                    code={TOKEN_ROUTE}
                    lang="ts"
                />
            ),
            note: "Each user only ever sees their own conversations.",
        },
        {
            title: "Send the user's token from your provider",
            extra: (
                <CodeSnippet
                    file="app/providers.tsx"
                    code={PROVIDER}
                    lang="ts"
                />
            ),
            note: "Your existing chat components work as-is. Tools that need approval arrive as interrupts (useInterrupt), see the docs.",
        },
    ];

    return (
        <>
            {picker}
            <ModeTabs
                terminal={<StepList steps={steps} />}
                agent={
                    <>
                        <div className="flex flex-col gap-1.5">
                            <div className="text-sm font-medium">
                                Paste this into your coding agent
                            </div>
                            <CommandBlock
                                command={skillPrompt(
                                    `connect my Next.js app to my Sectr agent with CopilotKit (the Copilot Runtime + SectrRunner setup). The agent's AG-UI endpoint is ${url}`,
                                )}
                                kind="prompt"
                            />
                        </div>
                        <div className="flex flex-col gap-2">
                            {keyControl(true)}
                            {!DEV_GATEWAY && (
                                <p className="text-xs text-muted-foreground">
                                    Put the key in{" "}
                                    <code className="font-mono">
                                        .env.local
                                    </code>{" "}
                                    as{" "}
                                    <code className="font-mono">
                                        SECTR_INVOCATION_KEY
                                    </code>{" "}
                                    yourself: don&apos;t paste it into the chat.
                                </p>
                            )}
                        </div>
                    </>
                }
            />
        </>
    );
}

/** Creates an invocation key for this project, inline. The key is held in
 *  the modal's state only: shown once, never stored. */
function KeyControl({
    app,
    keyValue,
    onKey,
    showKey,
}: {
    app: App;
    keyValue: string | null;
    onKey: (k: string) => void;
    /** Show the key itself (the Coding agent tab has no `.env.local` block). */
    showKey: boolean;
}) {
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string>();

    if (DEV_GATEWAY) {
        return (
            <p className="text-xs text-muted-foreground">
                No key needed under <code className="font-mono">sectr dev</code>
                .
            </p>
        );
    }
    if (keyValue) {
        return (
            <div className="flex flex-col gap-1.5">
                {showKey && (
                    <CommandBlock
                        command={`SECTR_INVOCATION_KEY=${keyValue}`}
                        kind="prompt"
                    />
                )}
                <p className="flex items-center gap-1.5 text-sm text-warning">
                    <TriangleAlert className="size-3.5 shrink-0" />
                    Copy it now: it&apos;s shown only once.
                </p>
            </div>
        );
    }
    return (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
            <Button
                size="sm"
                disabled={busy}
                onClick={async () => {
                    setBusy(true);
                    setError(undefined);
                    try {
                        const created = await api.createApiKey(
                            `${app.name} frontend`,
                            "invocation",
                            app.id,
                        );
                        onKey(created.key);
                    } catch (e) {
                        setError(
                            e instanceof Error
                                ? e.message
                                : "Couldn't create the key",
                        );
                    } finally {
                        setBusy(false);
                    }
                }}
            >
                {busy && <LoaderCircle className="animate-spin" />}
                Create invocation key
            </Button>
            <Link
                href="/settings"
                className="text-xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
            >
                or manage keys in Settings
            </Link>
            {error && <span className="text-xs text-destructive">{error}</span>}
        </div>
    );
}

const RUNTIME_ROUTE = `import {
  CopilotRuntime,
  createCopilotRuntimeHandler,
} from "@copilotkit/runtime/v2";
import { SectrAgent, SectrRunner } from "@sectr/copilotkit";

// Your agent on Sectr; the browser's Authorization header is forwarded.
const agents = {
  default: new SectrAgent({ url: process.env.SECTR_AGUI_URL!.replace(/\\/+$/, "") }),
};

// Keeps this server stateless: conversations live on Sectr.
const runtime = new CopilotRuntime({ agents, runner: new SectrRunner({ agents }) });

const handler = createCopilotRuntimeHandler({ runtime, basePath: "/api/copilotkit" });

export const GET = handler;
export const POST = handler;
export const PATCH = handler;
export const DELETE = handler;`;

const TOKEN_ROUTE = `// Mints a Sectr user token for the signed-in user. The invocation key
// is only used here; it never reaches the browser.
export async function GET() {
  // \`sectr dev\` needs no auth: run without a key locally.
  if (!process.env.SECTR_INVOCATION_KEY) return Response.json({ token: null });

  // Replace with your app's signed-in user id.
  const userId = /* your signed-in user's id */;
  if (!userId) return Response.json({ error: "not signed in" }, { status: 401 });

  const agui = new URL(process.env.SECTR_AGUI_URL!);
  const res = await fetch(new URL("/tokens", agui), {
    method: "POST",
    headers: {
      Authorization: \`Bearer \${process.env.SECTR_INVOCATION_KEY}\`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ app: agui.pathname.split("/")[2], external_user_id: userId }),
  });
  if (!res.ok) return Response.json({ error: await res.text() }, { status: 502 });
  const { token } = await res.json();
  return Response.json({ token });
}`;

const PROVIDER = `// Fetch the user's token once, e.g. in your Providers component:
const [token, setToken] = useState<string | null>();
useEffect(() => {
  fetch("/api/agent-token")
    .then((r) => r.json())
    .then((body) => setToken(body.token));
}, []);

// ...then pass it to your existing provider:
<CopilotKitProvider
  runtimeUrl="/api/copilotkit"
  headers={token ? { Authorization: \`Bearer \${token}\` } : {}}
>`;
