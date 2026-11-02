import { Fragment } from "react";
import { ChevronRight } from "lucide-react";
import Link from "next/link";

export interface Crumb {
  label: string;
  href?: string;
}

export function PageHeader({
  title,
  breadcrumbs,
  actions,
  children,
}: {
  title: React.ReactNode;
  breadcrumbs?: Crumb[];
  actions?: React.ReactNode;
  children?: React.ReactNode; // meta row under the title
}) {
  return (
    <div className="border-b border-border px-6 py-5">
      {breadcrumbs && breadcrumbs.length > 0 && (
        <nav className="mb-2 flex items-center gap-1 text-xs text-muted-foreground">
          {breadcrumbs.map((c, i) => (
            <Fragment key={i}>
              {i > 0 && <ChevronRight className="size-3 text-faint-foreground" />}
              {c.href ? (
                <Link
                  href={c.href}
                  className="transition-colors hover:text-foreground"
                >
                  {c.label}
                </Link>
              ) : (
                <span className="text-foreground">{c.label}</span>
              )}
            </Fragment>
          ))}
        </nav>
      )}
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-3xl font-semibold tracking-tight">{title}</h1>
        {actions && <div className="flex items-center gap-2">{actions}</div>}
      </div>
      {children && <div className="mt-1.5">{children}</div>}
    </div>
  );
}
