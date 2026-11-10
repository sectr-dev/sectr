"use client";

import { useEffect, useState } from "react";
import {
    ArrowRight,
    CircleCheck,
    CircleX,
    Bot,
    Check,
    Copy,
    KeyRound,
    LoaderCircle,
    Terminal,
    TriangleAlert,
} from "lucide-react";
import Link from "next/link";
import { toast } from "sonner";
import { BrandBackdrop } from "@/components/brand-backdrop";
import { ExploreLinks, WALKTHROUGH_URL } from "@/components/explore-links";
import {
    LangGraphLockup,
    OpenAIIcon,
    PydanticIcon,
} from "@/components/framework-icons";
import { LogoMark } from "@/components/logo";
import {
    CommandBlock,
    OPTIONS,
    StepsDialogContent,
    type FrameworkId,
    type Option,
} from "@/components/new-project-card";
import { TimelineStep } from "@/components/step-timeline";
import { Button, buttonVariants } from "@/components/ui/button";
import { Dialog, DialogTrigger } from "@/components/ui/dialog";
import * as api from "@/lib/api";
import { refreshApps } from "@/lib/hooks";
import type { App } from "@/lib/types";
import { ENDPOINT_GUIDE } from "@/lib/connections";
import { cn } from "@/lib/utils";
import { cliLoginCommand } from "@/lib/runtime-config";

/**
 * The empty org's overview: a welcome, not an empty grid. A new user has
 * no context and probably no CLI yet, so it walks them through, in order:
 *
 *   1. Install your tools — the CLI and/or the agent skill.
 *   2. Connect the CLI — `sectr auth login` (browser approval); an API key
 *      (shown once) stays as the manual route.
 *   3. Create your first agent — a starter or an existing agent, each
 *      opening its step modal (which never repeats steps 1–2); plus docs,
 *      samples and Discord (`ExploreLinks`, shared with the ghost card).
 *
 * Shown whenever the org has no projects (no welcome flag: the empty state
 * IS the welcome); the first deploy turns the page into the project grid.
 */
export function Welcome({
    onFirstProject,
}: {
    /** The first project is live and its modal closed: show the grid. */
    onFirstProject: () => void;
}) {
    return (
        <div className="relative isolate overflow-hidden pb-20">
            <BrandBackdrop strength="hero" />

            <div className="animate-in fade-in slide-in-from-bottom-2 mx-auto max-w-3xl px-6 pt-16 duration-700 motion-reduce:animate-none">
                {/* Hero */}
                <div className="flex flex-col items-center text-center">
                    <LogoMark
                        size="lg"
                        className="size-14 rounded-2xl shadow-[0_0_48px_rgb(240_154_82/45%)] [&>img]:size-11"
                    />
                    <h1 className="mt-6 text-5xl font-semibold tracking-tight sm:text-6xl">
                        Welcome to Sectr
                    </h1>
                    <p className="mt-4 max-w-xl text-lg leading-relaxed text-muted-foreground">
                        Let&apos;s get your first agent live. Set up your dev
                        environment, then start from a working starter or deploy
                        an agent you&apos;ve already built.
                    </p>
                    <a
                        href={WALKTHROUGH_URL}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="mt-3 text-sm text-muted-foreground underline-offset-2 transition-colors hover:text-foreground hover:underline"
                    >
                        See a video walkthrough →
                    </a>
                </div>

                {/* Steps */}
                <ol className="mt-16">
                    <TimelineStep
                        n={1}
                        title="Install your tools"
                        description="The Sectr CLI for your terminal, the agent skill for your coding agent. Or both."
                    >
                        {/* Stacked, not side by side: the install commands need the
                full width to stay on one line. */}
                        <div className="grid gap-3">
                            <ToolTile
                                icon={Terminal}
                                title="Sectr CLI"
                                description="Build, run and deploy from your terminal"
                                command="curl -fsSL https://sectr.dev/install.sh | sh"
                            />
                            <ToolTile
                                icon={Bot}
                                title="Agent skill"
                                description="Setup your coding agent to build and deploy Sectr agents for you"
                                command="npx skills add sectr-dev/skills"
                            />
                        </div>
                    </TimelineStep>

                    <TimelineStep
                        n={2}
                        title="Connect the CLI to your account"
                        description="Create a key, then sign in with it. Your coding agent uses the same login."
                    >
                        <ConnectCli />
                    </TimelineStep>

                    <TimelineStep
                        n={3}
                        title="Deploy your first agent"
                        description="Start from a template, or deploy an agent you already have."
                        last
                    >
                        <div className="grid gap-3 md:grid-cols-2">
                            <PathCard
                                option={OPTIONS.find((o) => o.id === "start")!}
                                onFirstProject={onFirstProject}
                                title="Start from a template"
                                description="A working support agent with tools and human approvals, ready to run and customize."
                            />
                            <PathCard
                                option={OPTIONS.find((o) => o.id === "deploy")!}
                                onFirstProject={onFirstProject}
                                title="Deploy an existing agent"
                                description="Already built one with the Sectr SDK? Ship it as a live endpoint."
                                frameworks
                            />
                        </div>
                        <p className="mt-3 text-xs text-muted-foreground">
                            Sectr runs your agents, or they can run on your own compute: your
                            AWS Lambda, Google Cloud Run or container platform.{" "}
                            <a
                                href={ENDPOINT_GUIDE}
                                target="_blank"
                                rel="noopener noreferrer"
                                className="underline underline-offset-2 transition-colors hover:text-foreground"
                            >
                                How it works
                            </a>
                        </p>
                        <ExploreLinks className="mt-5" />
                    </TimelineStep>
                </ol>
            </div>
        </div>
    );
}

function IconTile({
    icon: Icon,
    large = false,
}: {
    icon: React.ComponentType<{ className?: string }>;
    large?: boolean;
}) {
    return (
        <span
            className={cn(
                "flex shrink-0 items-center justify-center rounded-lg border border-brand/25 bg-brand/10 text-brand-foreground dark:text-brand",
                large ? "size-10" : "size-8",
            )}
        >
            <Icon className={large ? "size-5" : "size-4"} />
        </span>
    );
}

function ToolTile({
    icon,
    title,
    description,
    command,
}: {
    icon: React.ComponentType<{ className?: string }>;
    title: string;
    description: string;
    command: string;
}) {
    return (
        <div className="flex flex-col gap-3 rounded-xl border border-border bg-card p-4">
            <div className="flex items-start gap-3">
                <IconTile icon={icon} />
                <div className="min-w-0">
                    <div className="text-[15px] font-semibold">{title}</div>
                    <div className="text-sm text-muted-foreground">
                        {description}
                    </div>
                </div>
            </div>
            <CommandBlock
                command={command}
                className="mt-auto [&_code]:text-[13px]"
            />
        </div>
    );
}

/** `sectr auth login` (browser approval); an API key stays as the manual route. */
function ConnectCli() {
    const [key, setKey] = useState<string | null>(null);
    const [creating, setCreating] = useState(false);

    const create = async () => {
        setCreating(true);
        try {
            setKey((await api.createApiKey("CLI", "platform")).key);
        } catch (e) {
            toast.error("Key creation failed", {
                description: e instanceof Error ? e.message : String(e),
            });
        } finally {
            setCreating(false);
        }
    };

    // Sized to its content, not the column: a full-width banner or command
    // box with a short line of text in it looks stretched.
    return (
        <div className="w-fit max-w-full space-y-3">
            <div className="space-y-1.5">
                <CommandBlock
                    command={cliLoginCommand()}
                    className="[&_code]:text-[13px]"
                />
                <p className="text-sm text-muted-foreground">
                    Opens your browser so you can approve the login.
                </p>
            </div>
            {!key ? (
                <button
                    type="button"
                    onClick={() => void create()}
                    disabled={creating}
                    className="inline-flex items-center gap-1.5 text-sm text-muted-foreground underline underline-offset-2 hover:text-foreground disabled:opacity-60"
                >
                    {creating ? (
                        <LoaderCircle className="size-3.5 animate-spin" />
                    ) : (
                        <KeyRound className="size-3.5" />
                    )}
                    Or use an API key
                </button>
            ) : (
                <div className="space-y-3 rounded-xl border border-border bg-card p-4">
                    <div className="space-y-1.5">
                        <CommandBlock
                            command={key}
                            kind="prompt"
                            className="[&_code]:text-[13px]"
                        />
                        <p className="flex items-center gap-1.5 text-sm text-warning">
                            <TriangleAlert className="size-3.5 shrink-0" />
                            Copy it now: it&apos;s shown only once.
                        </p>
                    </div>
                    <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5 text-sm text-muted-foreground">
                        Then run
                        <InlineCommand command={cliLoginCommand("--with-key")} />
                        and paste the key when prompted.
                    </div>
                </div>
            )}
        </div>
    );
}

/** A command inline in a sentence: text-height, the whole chip copies. */
export function InlineCommand({ command }: { command: string }) {
    const [copied, setCopied] = useState(false);
    return (
        <button
            type="button"
            onClick={() => {
                void navigator.clipboard.writeText(command);
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
            }}
            aria-label={`Copy ${command}`}
            title="Copy"
            className="inline-flex items-center gap-1.5 rounded border border-border bg-surface px-1.5 py-0.5 font-mono text-[13px] text-foreground transition-colors hover:border-border-strong"
        >
            {command}
            {copied ? (
                <Check className="size-3 text-success" />
            ) : (
                <Copy className="size-3 text-faint-foreground" />
            )}
        </button>
    );
}

/** The supported frameworks as one quiet strip of marks, divided like a
 *  "works with" line (chips with borders don't fit the card on one line).
 *  LangGraph's official lockup already carries its name. Each opens the
 *  deploy modal with that framework selected. */
function FrameworkLogos({ onPick }: { onPick: (f: FrameworkId) => void }) {
    const divider = (
        <span className="h-4 w-[1.5px] shrink-0 rounded-full bg-border-strong" />
    );
    const chip =
        "pointer-events-auto relative flex items-center gap-1.5 rounded-sm transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-brand/60 focus-visible:outline-none";
    return (
        <div className="flex flex-nowrap items-center gap-2 whitespace-nowrap text-xs font-medium text-muted-foreground">
            <button
                type="button"
                className={chip}
                onClick={() => onPick("openai-agents")}
            >
                <OpenAIIcon className="size-3.5" />
                OpenAI
            </button>
            {divider}
            <button
                type="button"
                className={chip}
                aria-label="LangGraph"
                onClick={() => onPick("langgraph")}
            >
                <LangGraphLockup className="h-[18px] w-auto" />
            </button>
            {divider}
            <button
                type="button"
                className={chip}
                onClick={() => onPick("pydantic-ai")}
            >
                <PydanticIcon className="size-3.5" />
                Pydantic AI
            </button>
        </div>
    );
}

/** One of the welcome's two big cards, opening the option's step modal
 *  (the same modal as the header menu's, plus the first-deploy watcher).
 *  The whole card opens it
 *  (a stretched button underneath), except `frameworks`' chips, which are
 *  their own buttons on top and open it with that framework selected. */
function PathCard({
    option,
    title,
    description,
    frameworks = false,
    onFirstProject,
}: {
    option: Option;
    title: string;
    description: string;
    /** Show the framework chips (the deploy card). */
    frameworks?: boolean;
    onFirstProject: () => void;
}) {
    const [open, setOpen] = useState(false);
    const [live, setLive] = useState(false);
    const [framework, setFramework] = useState<FrameworkId>();
    return (
        <Dialog
            open={open}
            onOpenChange={(next) => {
                setOpen(next);
                // Closing after the first deploy went live: the page becomes the grid.
                if (!next && live) onFirstProject();
            }}
        >
            <div className="group relative flex flex-col rounded-xl border border-border bg-card p-5 text-left transition-colors hover:border-brand/60 hover:bg-brand/[0.04] has-[[data-slot=card-open]:focus-visible]:border-brand/60 has-[[data-slot=card-open]:focus-visible]:bg-brand/[0.04]">
                <DialogTrigger asChild>
                    <button
                        type="button"
                        data-slot="card-open"
                        aria-label={title}
                        onClick={() => setFramework(undefined)}
                        className="absolute inset-0 rounded-xl focus-visible:outline-none"
                    />
                </DialogTrigger>
                <div className="pointer-events-none relative flex flex-1 flex-col items-start gap-4">
                    <IconTile icon={option.icon} large />
                    <div>
                        <div className="text-lg font-semibold">{title}</div>
                        <p className="mt-1 text-[15px] leading-relaxed text-muted-foreground">
                            {description}
                        </p>
                    </div>
                    {frameworks && (
                        <FrameworkLogos
                            onPick={(f) => {
                                setFramework(f);
                                setOpen(true);
                            }}
                        />
                    )}
                    <span
                        className={cn(
                            buttonVariants({ size: "sm" }),
                            "mt-auto",
                        )}
                    >
                        {option.cta}
                        <ArrowRight className="transition-transform group-hover:translate-x-0.5" />
                    </span>
                </div>
            </div>
            <StepsDialogContent
                option={option}
                modal={option.modal}
                framework={framework}
                footer={<FirstDeployWatch onLive={() => setLive(true)} />}
            />
        </Dialog>
    );
}

/**
 * The welcome modals' footer: watches for the org's first deployment while
 * the modal is open (polling the project list), through waiting → building
 * → live (or failed, then keeps watching for the retry).
 */
function FirstDeployWatch({ onLive }: { onLive: () => void }) {
    const [app, setApp] = useState<App | null>(null);
    const status = app?.latest_deployment.status;

    useEffect(() => {
        let alive = true;
        const poll = async () => {
            try {
                const apps = await api.listApps();
                if (alive) setApp(apps[0] ?? null);
            } catch {
                // Transient: the next poll retries.
            }
        };
        void poll();
        const t = setInterval(() => void poll(), 3000);
        return () => {
            alive = false;
            clearInterval(t);
        };
    }, []);

    useEffect(() => {
        if (status === "active") onLive();
    }, [status, onLive]);

    const panel =
        "flex items-center gap-3 rounded-lg border px-4 py-3 transition-colors";

    if (app && status === "active") {
        return (
            <div className={cn(panel, "border-success/40 bg-success/10")}>
                <CircleCheck className="size-5 shrink-0 text-success" />
                <div className="min-w-0 flex-1">
                    <div className="text-sm font-semibold">
                        {app.name} is live!
                    </div>
                    <div className="text-xs text-muted-foreground">
                        Your first deployment is up and serving.
                    </div>
                </div>
                <Button size="sm" asChild>
                    {/* Leaving for the project page: refetch the project
                        list so the sidebar lists it there. */}
                    <Link href={`/apps/${app.id}`} onClick={refreshApps}>
                        Open project
                        <ArrowRight />
                    </Link>
                </Button>
            </div>
        );
    }
    if (app && status === "failed") {
        return (
            <div className={cn(panel, "border-danger/40 bg-danger/10")}>
                <CircleX className="size-5 shrink-0 text-danger" />
                <div className="min-w-0 flex-1">
                    <div className="text-sm font-semibold">
                        {app.name}&apos;s deploy failed
                    </div>
                    <div className="text-xs text-muted-foreground">
                        Check the{" "}
                        <code className="font-mono">sectr deploy</code> output,
                        fix it and deploy again. This keeps watching.
                    </div>
                </div>
            </div>
        );
    }
    return (
        <div
            className={cn(panel, "border-brand/30 bg-brand/[0.06]")}
            role="status"
            aria-live="polite"
        >
            <LoaderCircle className="size-5 shrink-0 animate-spin text-brand" />
            <div className="min-w-0 flex-1">
                <div className="text-sm font-semibold">
                    {app
                        ? `Building ${app.name}…`
                        : "Waiting for your first deployment…"}
                </div>
            </div>
        </div>
    );
}
