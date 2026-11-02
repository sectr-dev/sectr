"use client";

import { useEffect, useRef, useState } from "react";
import { ArrowUp } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * ChatInput — the send box for playground-originated sessions.
 * Enter sends, Shift+Enter newline. Disabled while the journal is
 * streaming or the turn is paused on an approval.
 */
export function ChatInput({
  onSend,
  disabled,
  disabledHint,
  className,
}: {
  onSend: (text: string) => void;
  disabled?: boolean;
  disabledHint?: string;
  className?: string;
}) {
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);

  // Focus on mount and whenever we become enabled again (e.g. turn done).
  useEffect(() => {
    if (!disabled) ref.current?.focus();
  }, [disabled]);

  const submit = () => {
    const t = text.trim();
    if (!t || disabled) return;
    onSend(t);
    setText("");
  };

  return (
    <div className={cn("shrink-0 border-t border-border px-4 py-3", className)}>
      <div
        className={cn(
          "flex items-end gap-2 rounded-lg border border-border bg-surface px-3 py-2 transition-colors focus-within:border-border-strong",
          disabled && "opacity-60",
        )}
      >
        <textarea
          ref={ref}
          rows={1}
          value={text}
          disabled={disabled}
          placeholder={
            disabled
              ? (disabledHint ?? "Unavailable for this session")
              : "Message this agent…"
          }
          onChange={(e) => {
            setText(e.target.value);
            e.target.style.height = "auto";
            e.target.style.height = `${Math.min(e.target.scrollHeight, 120)}px`;
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              submit();
            }
          }}
          className="max-h-[120px] min-h-6 flex-1 resize-none bg-transparent text-sm text-foreground outline-none placeholder:text-faint-foreground"
        />
        <button
          type="button"
          onClick={submit}
          disabled={disabled || !text.trim()}
          aria-label="Send message"
          className={cn(
            "flex size-7 shrink-0 items-center justify-center rounded-md transition-colors",
            !disabled && text.trim()
              ? "bg-brand text-background hover:bg-brand/90"
              : "bg-muted text-faint-foreground",
          )}
        >
          <ArrowUp className="size-4" />
        </button>
      </div>
      <div className="mt-1.5 px-1 text-[10px] text-faint-foreground">
        Enter to send · Shift+Enter for a new line
      </div>
    </div>
  );
}
