import * as api from "@/lib/api";
import type { ConsoleMode } from "@/lib/connections";

/** The dev gateway (`sectr dev`) runs the agent on this machine. */
export const CONSOLE_MODE: ConsoleMode = !api.MOCK_MODE && !api.PROD_MODE ? "dev" : "server";
