import { Badge } from "@/components/ui/badge";
import {
  DEPLOYMENT_STATUS_META,
  SESSION_STATUS_META,
} from "@/lib/events";
import type { DeploymentStatus, SessionStatus } from "@/lib/types";
import { cn } from "@/lib/utils";

export function SessionStatusBadge({
  status,
  className,
}: {
  status: SessionStatus;
  className?: string;
}) {
  const meta = SESSION_STATUS_META[status];
  return (
    <Badge variant={meta.variant} className={className}>
      <span className={cn("size-1.5 rounded-full", meta.dot)} />
      {meta.label}
    </Badge>
  );
}

export function DeploymentStatusBadge({
  status,
  className,
}: {
  status: DeploymentStatus;
  className?: string;
}) {
  const meta = DEPLOYMENT_STATUS_META[status];
  return (
    <Badge variant={meta.variant} className={className}>
      {meta.label}
    </Badge>
  );
}
