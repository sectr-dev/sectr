"use client";

import { Rocket } from "lucide-react";
import {
  CommandBlock,
  STEPS_CONTENT_CLASS,
  STEPS_OVERLAY_CLASS,
} from "@/components/new-project-card";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import { ENDPOINT_GUIDE } from "@/lib/connections";
import { cn } from "@/lib/utils";

/**
 * Deploy dialog — CLI-first, same story as New app: deploys ship from the
 * developer's terminal. Running `sectr deploy` in the app's project
 * directory builds and ships the next version; the console reflects it.
 * Opened from the project header, on every tab. Same placement and motion
 * as the step modals. A project that runs on its
 * own endpoint redeploys its image on its platform first; `sectr deploy`
 * then registers the new version.
 */
export function DeployDialog({
  appName,
  nextVersion,
  endpoint = false,
}: {
  appName: string;
  nextVersion: number;
  /** The project runs on the customer's own endpoint. */
  endpoint?: boolean;
}) {
  return (
    <Dialog>
      <DialogTrigger asChild>
        {/* Outline: it sits beside the header's primary action. */}
        <Button size="sm" variant="outline">
          <Rocket /> Deploy
        </Button>
      </DialogTrigger>
      <DialogContent
        overlayClassName={STEPS_OVERLAY_CLASS}
        className={cn(STEPS_CONTENT_CLASS, "sm:max-w-md")}
      >
        <DialogHeader>
          <DialogTitle className="text-xl">Deploy {appName}</DialogTitle>
          <DialogDescription>
            {endpoint ? (
              <>
                Redeploy your image on your compute provider, then run this in the
                project&apos;s directory to register{" "}
                <span className="font-mono text-xs">v{nextVersion}</span>.
              </>
            ) : (
              <>
                Run this in the project&apos;s directory to build and ship{" "}
                <span className="font-mono text-xs">v{nextVersion}</span>.
              </>
            )}
          </DialogDescription>
        </DialogHeader>
        <CommandBlock command="sectr deploy" />
        <p className="text-[11px] leading-relaxed text-faint-foreground">
          Sessions pin to the deployment they started on. Deploying a new
          version never interrupts live sessions.
          {endpoint && (
            <>
              {" "}
              See{" "}
              <a
                href={ENDPOINT_GUIDE}
                target="_blank"
                rel="noreferrer"
                className="underline underline-offset-2 hover:text-foreground"
              >
                running on your own compute
              </a>
              .
            </>
          )}
        </p>
      </DialogContent>
    </Dialog>
  );
}
