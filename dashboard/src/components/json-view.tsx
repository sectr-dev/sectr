import { useState } from "react";
import { ChevronRight } from "lucide-react";
import { cn } from "@/lib/utils";

/*
  Minimal JSON renderer with syntax coloring and collapsible nodes.
  Styled for the payload inspector: mono, tight, muted punctuation.
*/

const Key = ({ children }: { children: React.ReactNode }) => (
  <span className="text-info">{children}</span>
);
const Str = ({ children }: { children: React.ReactNode }) => (
  <span className="text-success">{children}</span>
);
const Lit = ({ children }: { children: React.ReactNode }) => (
  <span className="text-warning">{children}</span>
);

function Scalar({ value }: { value: unknown }) {
  if (typeof value === "string") return <Str>&quot;{value}&quot;</Str>;
  if (typeof value === "number") return <Lit>{value}</Lit>;
  if (typeof value === "boolean") return <Lit>{String(value)}</Lit>;
  if (value === null) return <Lit>null</Lit>;
  return null;
}

function Node({
  name,
  value,
  depth,
  initialOpen,
}: {
  name?: string;
  value: unknown;
  depth: number;
  initialOpen?: boolean;
}) {
  const [open, setOpen] = useState(initialOpen ?? depth < 2);
  const isArray = Array.isArray(value);
  const entries: [string, unknown][] = isArray
    ? value.map((v, i) => [String(i), v])
    : value != null && typeof value === "object"
      ? Object.entries(value as Record<string, unknown>)
      : [];

  const label = name !== undefined && <Key>&quot;{name}&quot;</Key>;

  // Scalars
  if (typeof value !== "object" || value === null) {
    return (
      <div className="leading-5">
        {label}
        {label !== undefined && <span className="text-faint-foreground">: </span>}
        <Scalar value={value} />
      </div>
    );
  }

  const openBrace = isArray ? "[" : "{";
  const closeBrace = isArray ? "]" : "}";
  const count = entries.length;

  return (
    <div className="leading-5">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="group inline cursor-pointer text-left"
      >
        <ChevronRight
          className={cn(
            "mr-0.5 inline size-3 text-faint-foreground transition-transform",
            open && "rotate-90",
          )}
        />
        {label}
        {label !== undefined && <span className="text-faint-foreground">: </span>}
        <span className="text-faint-foreground">
          {open ? openBrace : `${openBrace}…${closeBrace} ${isArray ? `${count} items` : count === 1 ? "1 key" : `${count} keys`}`}
        </span>
      </button>
      {open && (
        <>
          <div className="ml-3.5 border-l border-border pl-3">
            {entries.map(([k, v]) => (
              <Node key={k} name={k} value={v} depth={depth + 1} />
            ))}
          </div>
          <span className="text-faint-foreground">{closeBrace}</span>
        </>
      )}
    </div>
  );
}

export function JsonView({ data, className }: { data: unknown; className?: string }) {
  return (
    <div className={cn("font-mono text-xs", className)}>
      <Node value={data} depth={0} initialOpen />
    </div>
  );
}
