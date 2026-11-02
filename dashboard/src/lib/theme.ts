"use client";

/*
  Theme controller — light / dark / system, persisted per browser.

  Mechanism: a `.dark` class on <html> flips the CSS custom properties
  (globals.css). A pre-hydration script in the root layout applies the
  stored choice BEFORE first paint (no light-flash); this module is the
  runtime half — read, set, and react to the OS changing underneath
  "system". Token contract: components never know a theme exists.
*/

import { useCallback, useEffect, useState } from "react";

export type Theme = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

const KEY = "sectr.theme";

function systemPrefersDark(): boolean {
  return typeof window !== "undefined" && window.matchMedia("(prefers-color-scheme: dark)").matches;
}

/** Apply a theme to <html> (and persist it). The pre-hydration script
 *  in layout.tsx does the same job before React exists. */
export function applyTheme(theme: Theme): void {
  const dark = theme === "dark" || (theme === "system" && systemPrefersDark());
  document.documentElement.classList.toggle("dark", dark);
  try {
    localStorage.setItem(KEY, theme);
  } catch {
    // private mode etc. — the choice just doesn't persist
  }
}

export function useTheme(): {
  theme: Theme;
  resolved: ResolvedTheme;
  setTheme: (t: Theme) => void;
} {
  const [theme, setThemeState] = useState<Theme>("system");
  const [resolved, setResolved] = useState<ResolvedTheme>("light");

  // Initial read (client-only) + follow OS changes while in "system".
  useEffect(() => {
    let t: Theme = "system";
    try {
      const stored = localStorage.getItem(KEY);
      if (stored === "light" || stored === "dark" || stored === "system") t = stored;
    } catch {
      // no storage — system default
    }
    const sync = () => {
      setThemeState(t);
      const dark = t === "dark" || (t === "system" && systemPrefersDark());
      document.documentElement.classList.toggle("dark", dark);
      setResolved(dark ? "dark" : "light");
    };
    // Deferred one tick — no setState inside the effect body (cascading-render rule).
    const timer = setTimeout(sync, 0);

    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    mq.addEventListener("change", sync);
    return () => {
      clearTimeout(timer);
      mq.removeEventListener("change", sync);
    };
  }, []);

  const setTheme = useCallback((t: Theme) => {
    setThemeState(t);
    applyTheme(t);
    const dark = t === "dark" || (t === "system" && systemPrefersDark());
    setResolved(dark ? "dark" : "light");
  }, []);

  return { theme, resolved, setTheme };
}
