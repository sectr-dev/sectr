"use client";

import { Fragment, useEffect, useRef, useState } from "react";
import {
    ArrowRight,
    Check,
    ChevronDown,
    Copy,
    FolderPlus,
    Plus,
    Rocket,
} from "lucide-react";
import { ExploreLinks, WALKTHROUGH_URL } from "@/components/explore-links";
import {
    LangGraphIcon,
    OpenAIIcon,
    PydanticIcon,
} from "@/components/framework-icons";
import { HighlightedCode } from "@/components/highlighted-code";
import {
    CloudRunIcon,
    ContainerIcon,
    LambdaIcon,
    RunsOnMark,
} from "@/components/platform-icons";
import { TimelineStep } from "@/components/step-timeline";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogHeader,
    DialogTitle,
} from "@/components/ui/dialog";
import {
    DropdownMenu,
    DropdownMenuContent,
    DropdownMenuSeparator,
    DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { DropdownMenu as DropdownMenuPrimitive } from "radix-ui";
import { ENDPOINT_GUIDE } from "@/lib/connections";
import { cn } from "@/lib/utils";

const DOCS = "https://docs.sectr.dev";

export interface Step {
    title: string;
    /** Shown in a copyable block. */
    command?: string;
    /** Anything else for the step (a picker, a code snippet), above the command. */
    extra?: React.ReactNode;
    /** A shell command (`$` prefix) or free text to paste somewhere else. */
    kind?: "shell" | "prompt";
    note?: string;
}

/** What a menu row shows; the icon is a lucide icon or a brand mark. */
export interface MenuOption {
    icon: React.ComponentType<{ className?: string }>;
    title: string;
    description: string;
}

export interface Option extends MenuOption {
    id: "start" | "deploy";
    cta: string;
    /** The step modal's header and docs link (its steps are StartBody's /
     *  DeployBody's). */
    modal: { description: string; docsHref: string };
}

export const OPTIONS: Option[] = [
    {
        id: "start",
        icon: FolderPlus,
        title: "Start from a template",
        description: "Scaffold a working agent with the Sectr CLI",
        cta: "Start",
        modal: {
            description:
                "Scaffold, run and deploy a working support agent: yourself in a terminal, or by handing it to your coding agent.",
            docsHref: `${DOCS}/getting-started/first-agent/`,
        },
    },
    {
        id: "deploy",
        icon: Rocket,
        title: "Deploy existing agent",
        description: "Ship an agent you've already built",
        cta: "Deploy",
        modal: {
            description:
                "Add Sectr to the agent you've already built, then deploy it: yourself in a terminal, or by handing it to your coding agent.",
            docsHref: `${DOCS}/getting-started/first-agent/#deploy-your-agent`,
        },
    },
];

/**
 * A header menu of options, as glyph + title + description rows with an
 * optional footer. Click to open (like Vercel's "Add New"), not hover: hover
 * menus misfire on the way past, and don't exist on touch. What a pick opens
 * (a step modal) lives outside the menu, in the caller's `onPick`, once the
 * menu has closed. Used by "Create new ▾" and "Connect your frontend ▾".
 */
export function OptionMenu<T extends MenuOption>({
    trigger,
    options,
    onPick,
    footer,
}: {
    trigger: React.ReactNode;
    options: readonly T[];
    onPick: (option: T) => void;
    footer?: React.ReactNode;
}) {
    return (
        /* Non-modal: a modal menu handing focus to a dialog can leave the
           page's pointer-events locked. */
        <DropdownMenu modal={false}>
            <DropdownMenuTrigger asChild>{trigger}</DropdownMenuTrigger>
            {/* Same option rows as the ghost card. The rows are bare Radix
                items (the styled DropdownMenuItem forces every descendant to
                one text color on focus). */}
            {/* Lifted well off the page: a stronger ring and a deep
                shadow, and nudged 8px past the trigger's right edge,
                which otherwise lines up exactly with the grid's right
                edge and makes the menu read as part of the page. */}
            <DropdownMenuContent
                align="end"
                alignOffset={-8}
                sideOffset={8}
                className="@container w-[24rem] p-1.5 shadow-[0_14px_28px_-8px_rgb(0_0_0/0.18),0_4px_10px_-4px_rgb(0_0_0/0.12)] ring-foreground/15 dark:shadow-[0_14px_28px_-8px_rgb(0_0_0/0.45),0_4px_10px_-4px_rgb(0_0_0/0.32)]"
            >
                <div className="flex flex-col">
                    {options.map((o, i) => (
                        <Fragment key={o.title}>
                            {i > 0 && (
                                <DropdownMenuSeparator className="mx-3 my-1" />
                            )}
                            <DropdownMenuPrimitive.Item
                                onSelect={() => onPick(o)}
                                className={cn(
                                    OPTION_ROW,
                                    "cursor-default border-transparent bg-transparent shadow-none",
                                )}
                            >
                                <OptionBody option={o} />
                            </DropdownMenuPrimitive.Item>
                        </Fragment>
                    ))}
                </div>
                {footer && (
                    <>
                        <DropdownMenuSeparator className="my-1.5" />
                        {footer}
                    </>
                )}
            </DropdownMenuContent>
        </DropdownMenu>
    );
}

/** The overview header's "Create new ▾" menu: the creation options plus the
 *  explore links. */
export function NewProjectMenu() {
    const [active, setActive] = useState<Option | null>(null);
    const [dialogOpen, setDialogOpen] = useState(false);
    return (
        <>
            <OptionMenu
                trigger={
                    <Button size="sm">
                        Create new
                        <ChevronDown />
                    </Button>
                }
                options={OPTIONS}
                onPick={(o) => {
                    setActive(o);
                    setDialogOpen(true);
                }}
                footer={
                    <ExploreLinks
                        size="sm"
                        menuItems
                        className="justify-start px-1.5 pb-1"
                    />
                }
            />
            <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
                {active && (
                    <StepsDialogContent option={active} modal={active.modal} />
                )}
            </Dialog>
        </>
    );
}

/**
 * The grid's ghost card: a dashed, project-card-sized "+ Create new
 * project". On hover (or focus, or a tap) it turns into the same options as
 * the header menu (two option buttons and the explore links), in place and within its
 * own bounds, so
 * it never stretches its row. Same dialogs as the menu.
 */
export function NewProjectGhostCard() {
    const [hovered, setHovered] = useState(false);
    const [pinned, setPinned] = useState(false);
    const [active, setActive] = useState<Option | null>(null);
    const [dialogOpen, setDialogOpen] = useState(false);
    const expanded = hovered || pinned;
    const gridRef = useRef<HTMLDivElement>(null);

    // Keyboard / tap expansion swaps the label for the tiles: hand focus to
    // the first tile.
    useEffect(() => {
        if (pinned)
            gridRef.current?.querySelector<HTMLElement>("button, a")?.focus();
    }, [pinned]);

    return (
        <div
            onMouseEnter={() => setHovered(true)}
            onMouseLeave={() => {
                setHovered(false);
                setPinned(false);
            }}
            onBlur={(e) => {
                // Focus (or a tap) moving outside the card collapses a pinned card.
                if (!e.currentTarget.contains(e.relatedTarget))
                    setPinned(false);
            }}
            className={cn(
                "@container relative flex h-full min-h-36 w-full rounded-xl border border-dashed p-3 transition-colors duration-200",
                expanded
                    ? "border-brand/60 bg-brand/[0.04]"
                    : "border-border-strong",
            )}
        >
            {/* Both layers are always mounted and cross-fade, so the card
          animates instead of swapping. The hidden one is inert (no focus,
          no clicks, hidden from assistive tech). */}
            <button
                type="button"
                onClick={() => setPinned(true)}
                onFocus={() => setPinned(true)}
                tabIndex={expanded ? -1 : 0}
                aria-hidden={expanded}
                className={cn(
                    "absolute inset-0 flex items-center justify-center gap-2 text-sm font-medium text-muted-foreground transition-[opacity,transform] duration-150 motion-reduce:transition-none",
                    expanded && "pointer-events-none scale-95 opacity-0",
                )}
            >
                <Plus className="size-4" />
                Create new project
            </button>
            {/* Content-sized and centered in the card, not stretched over
                it: the two ways to create a project as compact option
                buttons (side by side when the card is wide enough, stacked
                and title-only when not), then the welcome's quiet
                "Or explore" links. */}
            <div
                ref={gridRef}
                inert={!expanded}
                className={cn(
                    "flex flex-1 flex-col items-center justify-center gap-3 transition-opacity duration-300 motion-reduce:transition-none",
                    expanded ? "opacity-100" : "opacity-0",
                )}
            >
                <div className="grid w-full max-w-xl grid-cols-1 gap-2 @[31.5rem]:grid-cols-2">
                    {OPTIONS.map((o) => (
                        <button
                            key={o.title}
                            type="button"
                            className={cn(OPTION_ROW, "py-2 @[31.5rem]:py-2.5")}
                            onClick={() => {
                                setActive(o);
                                setDialogOpen(true);
                            }}
                        >
                            <OptionBody
                                option={o}
                                descriptionClass="hidden @[31.5rem]:line-clamp-2"
                            />
                        </button>
                    ))}
                </div>
                <ExploreLinks size="sm" />
            </div>
            <Dialog open={dialogOpen} onOpenChange={setDialogOpen}>
                {active && (
                    <StepsDialogContent option={active} modal={active.modal} />
                )}
            </Dialog>
        </div>
    );
}

/** An option as a row: the ghost card's buttons and the menu's items. */
const OPTION_ROW =
    "group/opt flex min-w-0 items-center gap-3 rounded-lg border border-border bg-card px-3 py-2.5 text-left shadow-xs outline-none transition-colors hover:border-brand/60 hover:bg-brand/[0.06] focus-visible:border-brand/60 focus-visible:bg-brand/[0.06] data-highlighted:border-brand/60 data-highlighted:bg-brand/[0.06]";

function OptionBody({
    option: o,
    descriptionClass = "line-clamp-2",
}: {
    option: MenuOption;
    descriptionClass?: string;
}) {
    return (
        <>
            <span className="flex size-8 shrink-0 items-center justify-center rounded-md border border-brand/25 bg-brand/10 text-brand-foreground dark:text-brand">
                <o.icon className="size-4" />
            </span>
            <span className="min-w-0 flex-1">
                <span className="block truncate text-sm font-semibold">
                    {o.title}
                </span>
                <span
                    className={cn(
                        "mt-0.5 text-xs leading-snug text-muted-foreground",
                        descriptionClass,
                    )}
                >
                    {o.description}
                </span>
            </span>
            <ArrowRight className="size-3.5 shrink-0 text-faint-foreground transition-[color,translate] group-hover/opt:translate-x-0.5 group-hover/opt:text-brand group-data-highlighted/opt:translate-x-0.5 group-data-highlighted/opt:text-brand" />
        </>
    );
}

/** An option's step modal (inside a `<Dialog>`). The same everywhere it
 *  opens (welcome, header menu, ghost card): no install / sign-in steps,
 *  since the welcome covers those up front and anyone else already has
 *  the CLI. */
/** The step modals' placement and motion, shared with the project's Deploy
 *  dialog: pinned near the top and centered on the content area (right of
 *  the sidebar, via `--sidebar-w` from AppShell), rising in with a matching
 *  backdrop fade; scrolls on short screens. */
export const STEPS_OVERLAY_CLASS = "bg-black/20 duration-300";
export const STEPS_CONTENT_CLASS =
    "left-[calc(50%+var(--sidebar-w,0px)/2)] top-[max(1.5rem,15vh)] max-h-[calc(100dvh-max(1.5rem,15vh)-1.5rem)] translate-y-0 overflow-y-auto p-6 duration-300 data-open:slide-in-from-bottom-10 data-open:zoom-in-100 data-closed:zoom-out-100";

export function StepsDialogContent({
    option,
    modal,
    footer,
    framework,
}: {
    option: Option;
    modal: NonNullable<Option["modal"]>;
    /** Under the steps (the welcome's first-deploy watcher). */
    footer?: React.ReactNode;
    /** Preselects the Deploy modal's framework picker. */
    framework?: FrameworkId;
}) {
    // Pinned near the top instead of centered: the height changes (tabs, the
    // deploy watcher), and a centered dialog would shift with it. Centered on
    // the content area, right of the sidebar (--sidebar-w, set by AppShell). Scrolls on
    // short screens rather than running off them.
    return (
        <DialogContent
            overlayClassName={STEPS_OVERLAY_CLASS}
            className={cn(STEPS_CONTENT_CLASS, "sm:max-w-2xl")}
        >
            <DialogHeader>
                <DialogTitle className="text-xl">{option.title}</DialogTitle>
                <DialogDescription>{modal.description}</DialogDescription>
            </DialogHeader>
            {option.id === "start" ? (
                <StartBody />
            ) : (
                <DeployBody initialFramework={framework} />
            )}
            {footer}
            <div className="flex flex-wrap gap-x-4 gap-y-1">
                <a
                    href={modal.docsHref}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-xs text-muted-foreground underline-offset-2 transition-colors hover:text-foreground hover:underline"
                >
                    Read the docs →
                </a>
                <a
                    href={WALKTHROUGH_URL}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-xs text-muted-foreground underline-offset-2 transition-colors hover:text-foreground hover:underline"
                >
                    See a video walkthrough →
                </a>
            </div>
        </DialogContent>
    );
}

export function StepList({ steps }: { steps: Step[] }) {
    return (
        <ol>
            {steps.map((s, i) => (
                <TimelineStep
                    key={s.title}
                    n={i + 1}
                    title={s.title}
                    size="sm"
                    last={i === steps.length - 1}
                >
                    {s.extra}
                    {s.command && (
                        <CommandBlock command={s.command} kind={s.kind} />
                    )}
                    {s.note && (
                        <p className="mt-1.5 text-xs text-muted-foreground">
                            {s.note}
                        </p>
                    )}
                </TimelineStep>
            ))}
        </ol>
    );
}

const FRAMEWORKS = [
    {
        id: "openai-agents",
        label: "OpenAI Agents SDK",
        icon: <OpenAIIcon className="size-4" />,
    },
    { id: "langgraph", label: "LangGraph", icon: <LangGraphIcon className="size-4" /> },
    { id: "pydantic-ai", label: "Pydantic AI", icon: <PydanticIcon className="size-4" /> },
] as const;
export type FrameworkId = (typeof FRAMEWORKS)[number]["id"];

/** Adding Sectr to an existing agent, per framework: the install and the
 *  whole integration (from the marketing site's snippets, trimmed to the
 *  Sectr lines; `agent` / `graph` is the user's own). */
const WIRING: Record<FrameworkId, { install: string; code: string }> = {
    "openai-agents": {
        install: 'uv add "sectr[openai-agents]"',
        code: `from sectr import AgentApp
from sectr.adapters.openai_agents import openai_agents

# agent = Agent(...)  <- your existing agent
app = AgentApp(name="my-agent")
app.add_route("/chat", openai_agents(agent))`,
    },
    langgraph: {
        install: 'uv add "sectr[langgraph]"',
        code: `from sectr import AgentApp
from sectr.adapters.langgraph import langgraph

# graph = ....compile()  <- your existing graph
app = AgentApp(name="my-agent")
app.add_route("/chat", langgraph(graph))`,
    },
    "pydantic-ai": {
        install: 'uv add "sectr[pydantic-ai]"',
        code: `from sectr import AgentApp
from sectr.adapters.pydantic_ai import pydantic_ai

# agent = Agent(...)  <- your existing agent
app = AgentApp(name="my-agent")
app.add_route("/chat", pydantic_ai(agent))`,
    },
};

const MANIFEST = `[app]
name = "my-agent"

[agent]
entry = "app:app"
python = "3.12"`;

/** The prompt for the coding-agent tabs. */
export function skillPrompt(task: string): string {
    return (
        `Use the Sectr skill to ${task}. ` +
        "If the skill isn't installed, run `npx skills add sectr-dev/skills` first."
    );
}

/** A row of mutually exclusive options. */
export function Segmented<T extends string>({
    label,
    value,
    options,
    onChange,
}: {
    label: string;
    value: T;
    options: readonly { id: T; label: string; icon?: React.ReactNode }[];
    onChange: (v: T) => void;
}) {
    return (
        <div role="radiogroup" aria-label={label}>
            <div className="mb-1.5 text-xs font-medium text-muted-foreground">
                {label}
            </div>
            <div className="flex flex-wrap gap-1.5">
                {options.map((o) => (
                    <button
                        key={o.id}
                        type="button"
                        role="radio"
                        aria-checked={value === o.id}
                        onClick={() => onChange(o.id)}
                        className={cn(
                            "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1 text-xs font-medium transition-colors",
                            value === o.id
                                ? "border-brand/60 bg-brand/10 text-foreground"
                                : "border-border bg-surface text-muted-foreground hover:border-border-strong hover:text-foreground",
                        )}
                    >
                        {o.icon}
                        {o.label}
                    </button>
                ))}
            </div>
        </div>
    );
}

/** Where the agent runs: Sectr Run, or the developer's own platform
 *  (`sectr init --endpoint <platform>`). */
const RUNS_ON = [
    {
        id: "sectr",
        label: "Sectr Run",
        icon: <RunsOnMark kind="sectr" bare className="size-4 [&>img]:size-4" />,
    },
    { id: "lambda", label: "Your AWS Lambda", icon: <LambdaIcon className="size-4" /> },
    { id: "cloud-run", label: "Your Cloud Run", icon: <CloudRunIcon className="size-4" /> },
    {
        id: "container",
        label: "Your container platform",
        icon: <ContainerIcon className="size-4" />,
    },
] as const;
type RunsOnId = (typeof RUNS_ON)[number]["id"];

/** "Lambda", "Cloud Run", "container platform": a runs-on choice in a
 *  sentence. */
function platformName(r: RunsOnId): string {
    return RUNS_ON.find((o) => o.id === r)!.label.replace(/^Your /, "");
}

/** The deploy snippet for sectr.toml: `sectr init --endpoint` adds the key
 *  empty, and the developer fills it in once their image is deployed. */
const ENDPOINT_TOML = `[deploy]
endpoint = "https://…"`;

/** The last steps for an agent on the developer's own platform, in both
 *  the Start and Deploy modals: deploy the image there, put its URL in
 *  sectr.toml, then register it. */
function endpointDeploySteps(platform: string): Step[] {
    return [
        {
            title: `Deploy it on your ${platform}`,
            extra: (
                <p className="text-xs text-muted-foreground">
                    Build the image from the generated Dockerfile and deploy it
                    there (init prints the steps;{" "}
                    <a
                        href={ENDPOINT_GUIDE}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="underline underline-offset-2 hover:text-foreground"
                    >
                        full guide
                    </a>
                    ).
                </p>
            ),
        },
        {
            title: "Put its URL in sectr.toml",
            extra: (
                <CodeSnippet
                    file="sectr.toml"
                    code={ENDPOINT_TOML}
                    lang="toml"
                />
            ),
            note: "The URL your platform gave the deployed image: your compute endpoint.",
        },
        {
            title: "Register it with Sectr",
            command: "sectr deploy",
            note: "Sectr wakes your compute endpoint for each turn. It shows up here once it's live.",
        },
    ];
}

/** The framework + chat-frontend choice behind `sectr init`. */
function AgentChoice({
    framework,
    frontend,
    runsOn,
    onFramework,
    onFrontend,
    onRunsOn,
}: {
    framework: FrameworkId;
    /** Omit both to show only the framework row. */
    frontend?: boolean;
    /** Omit both to leave out the where-it-runs row. */
    runsOn?: RunsOnId;
    onFramework: (f: FrameworkId) => void;
    onFrontend?: (b: boolean) => void;
    onRunsOn?: (r: RunsOnId) => void;
}) {
    return (
        <div className="flex flex-col gap-3">
            <Segmented
                label="Framework"
                value={framework}
                options={FRAMEWORKS}
                onChange={onFramework}
            />
            {onFrontend && (
                <Segmented
                    label="CopilotKit chat frontend"
                    value={frontend ? "yes" : "no"}
                    options={[
                        { id: "yes", label: "Yes" },
                        { id: "no", label: "No" },
                    ]}
                    onChange={(v) => onFrontend(v === "yes")}
                />
            )}
            {onRunsOn && runsOn && (
                <Segmented
                    label="Where will it run?"
                    value={runsOn}
                    options={RUNS_ON}
                    onChange={onRunsOn}
                />
            )}
        </div>
    );
}

/**
 * The Start-from-a-template modal body: two ways to the same agent, as
 * tabs — the terminal steps, or one prompt to paste into a coding agent.
 * The framework / frontend choice is shared between them.
 */
function StartBody() {
    const [framework, setFramework] = useState<FrameworkId>("openai-agents");
    const [frontend, setFrontend] = useState(false);
    const [runsOn, setRunsOn] = useState<RunsOnId>("sectr");
    const label = FRAMEWORKS.find((f) => f.id === framework)!.label;
    const choice = (
        <AgentChoice
            framework={framework}
            frontend={frontend}
            runsOn={runsOn}
            onFramework={setFramework}
            onFrontend={setFrontend}
            onRunsOn={setRunsOn}
        />
    );
    const endpoint = runsOn !== "sectr";
    const platform = platformName(runsOn);

    // With the chat frontend the agent lives in my-agent/agent/, and that's
    // where `sectr dev` and `sectr deploy` run.
    const dir = frontend ? "my-agent/agent" : "my-agent";
    const init =
        `sectr init my-agent --template ${framework}` +
        (frontend ? " --frontend copilotkit" : "") +
        (endpoint ? ` --endpoint ${runsOn}` : "") +
        ` && cd ${dir}`;
    const steps: Step[] = [
        {
            title: "Create your agent",
            extra: <div className="mb-3">{choice}</div>,
            command: init,
            note: frontend
                ? "Scaffolds the agent and the chat app, then moves into the agent's folder."
                : "Scaffolds a working support agent, then moves into it.",
        },
        {
            title: "Run it locally",
            command: "sectr dev",
            note: "Chat with it in your browser while you iterate.",
        },
        ...(endpoint
            ? endpointDeploySteps(platform)
            : [
                  {
                      title: "Deploy it",
                      command: "sectr deploy",
                      note: "Ships it as a live endpoint. It shows up here once it's live.",
                  },
              ]),
    ];

    return (
        <ModeTabs
            terminal={<StepList steps={steps} />}
            agent={
                <>
                    {choice}
                    <PromptBlock
                        command={skillPrompt(
                            `build and deploy a support agent with ${label}${
                                frontend
                                    ? " and a CopilotKit chat frontend"
                                    : ""
                            }${endpoint ? `, running on my own ${platform}` : ""}`,
                        )}
                    />
                </>
            }
        />
    );
}

/**
 * The Deploy-an-existing-agent modal body: the same two modes as Start.
 * Manual shows how to wire the user's framework into Sectr (the actual
 * lines), then the manifest and the deploy; for the developer's own
 * platform, `sectr init --endpoint` adds the image's files first.
 */
function DeployBody({ initialFramework }: { initialFramework?: FrameworkId }) {
    const [framework, setFramework] = useState<FrameworkId>(
        initialFramework ?? "openai-agents",
    );
    const [runsOn, setRunsOn] = useState<RunsOnId>("sectr");
    const choice = (
        <AgentChoice
            framework={framework}
            runsOn={runsOn}
            onFramework={setFramework}
            onRunsOn={setRunsOn}
        />
    );
    const wiring = WIRING[framework];
    const endpoint = runsOn !== "sectr";
    const platform = platformName(runsOn);

    const steps: Step[] = [
        {
            title: "Add Sectr to your agent",
            extra: (
                <div className="flex flex-col gap-2">
                    <div className="mb-1">{choice}</div>
                    <CommandBlock command={wiring.install} />
                    <CodeSnippet
                        file="app.py"
                        code={wiring.code}
                        lang="python"
                    />
                </div>
            ),
            note: "Your agent stays ordinary framework code; this serves it at /chat.",
        },
        {
            title: "Add a Sectr manifest",
            extra: (
                <CodeSnippet file="sectr.toml" code={MANIFEST} lang="toml" />
            ),
            note: "Next to app.py. entry points at the AgentApp above.",
        },
        ...(endpoint
            ? [
                  {
                      title: "Add the files for your platform",
                      command: `sectr init --endpoint ${runsOn}`,
                      note: "Run it next to sectr.toml. Adds a Dockerfile and .dockerignore, and [deploy] endpoint to sectr.toml.",
                  },
                  ...endpointDeploySteps(platform),
              ]
            : [
                  {
                      title: "Deploy",
                      command: "sectr deploy",
                      note: "Builds it and creates the project. It shows up here once it's live.",
                  },
              ]),
    ];

    return (
        <ModeTabs
            terminal={<StepList steps={steps} />}
            agent={
                <>
                    <Segmented
                        label="Where will it run?"
                        value={runsOn}
                        options={RUNS_ON}
                        onChange={setRunsOn}
                    />
                    <PromptBlock
                        command={skillPrompt(
                            "deploy my existing agent to Sectr, detecting its framework (OpenAI Agents SDK, LangGraph or Pydantic AI) from the project" +
                                (endpoint ? `, running on my own ${platform}` : ""),
                        )}
                    />
                </>
            }
        />
    );
}

/** Manual / Coding agent tabs, shared by the Start and Deploy modals. */
export function ModeTabs({
    terminal,
    agent,
}: {
    terminal: React.ReactNode;
    agent: React.ReactNode;
}) {
    return (
        <Tabs defaultValue="terminal" className="gap-4">
            <TabsList>
                <TabsTrigger value="terminal">Manual</TabsTrigger>
                <TabsTrigger value="agent">Coding agent</TabsTrigger>
            </TabsList>
            <TabsContent value="terminal">{terminal}</TabsContent>
            <TabsContent value="agent" className="flex flex-col gap-4">
                {agent}
            </TabsContent>
        </Tabs>
    );
}

function PromptBlock({ command }: { command: string }) {
    return (
        <div>
            <div className="mb-1.5 text-sm font-medium">
                Paste this into your coding agent
            </div>
            <CommandBlock command={command} kind="prompt" />
        </div>
    );
}

/** A small multi-line file (filename header with a copy button). Only the
 *  header button copies, so the code itself stays selectable. */
export function CodeSnippet({
    file,
    code,
    lang,
}: {
    file: string;
    code: string;
    lang: "python" | "toml" | "ts" | "dotenv";
}) {
    const [copied, setCopied] = useState(false);
    const copy = () => {
        void navigator.clipboard.writeText(code);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
    };
    return (
        <div className="block w-full overflow-hidden rounded-md border border-border bg-surface text-left">
            <div className="flex items-center justify-between border-b border-border py-1 pr-1.5 pl-3 font-mono text-[11px] text-muted-foreground">
                {file}
                <button
                    type="button"
                    onClick={copy}
                    aria-label={`Copy ${file}`}
                    title="Copy"
                    className="flex size-6 items-center justify-center rounded-md transition-colors hover:bg-surface-raised hover:text-foreground"
                >
                    {copied ? (
                        <Check className="size-3.5 text-success" />
                    ) : (
                        <Copy className="size-3.5" />
                    )}
                </button>
            </div>
            <div className="block overflow-x-auto px-3 py-2.5 font-mono text-xs leading-relaxed whitespace-pre text-foreground select-text">
                <HighlightedCode code={code} lang={lang} />
            </div>
        </div>
    );
}

/** A copyable command (`$` prefix) or prompt; click anywhere to copy. */
export function CommandBlock({
    command,
    kind = "shell",
    className,
}: {
    command: string;
    kind?: Step["kind"];
    className?: string;
}) {
    const [copied, setCopied] = useState(false);
    const copy = () => {
        void navigator.clipboard.writeText(command);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
    };
    // The whole box is the copy button, not just the icon.
    return (
        <button
            type="button"
            onClick={copy}
            aria-label={`Copy ${command}`}
            title="Copy"
            className={cn(
                "group/cmd flex w-full items-center gap-2 rounded-md border border-border bg-surface px-3 py-2 text-left transition-colors hover:border-border-strong hover:bg-surface-raised",
                className,
            )}
        >
            <code className="min-w-0 flex-1 break-words font-mono text-xs text-foreground">
                {kind !== "prompt" && (
                    <span className="mr-1.5 text-success">$</span>
                )}
                {command}
            </code>
            <span className="flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors group-hover/cmd:text-foreground">
                {copied ? (
                    <Check className="size-4 text-success" />
                ) : (
                    <Copy className="size-4" />
                )}
            </span>
        </button>
    );
}
