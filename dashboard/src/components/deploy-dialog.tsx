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
import { cn } from "@/lib/utils";

/**
 * Deploy dialog — CLI-first, same story as New app: deploys ship from the
 * developer's terminal. Running `sectr deploy` in the app's project
 * directory builds and ships the next version; the console reflects it.
 * Same placement and motion as the step modals.
 */
export function DeployDialog({
  appName,
  nextVersion,
}: {
  appName: string;
  nextVersion: number;
}) {
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button size="sm">
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
            Run this in the project&apos;s directory to build and ship{" "}
            <span className="font-mono text-xs">v{nextVersion}</span>.
          </DialogDescription>
        </DialogHeader>
        <CommandBlock command="sectr deploy" />
        <p className="text-[11px] leading-relaxed text-faint-foreground">
          Sessions pin to the deployment they started on. Deploying a new
          version never interrupts live sessions.
        </p>
      </DialogContent>
    </Dialog>
  );
}
