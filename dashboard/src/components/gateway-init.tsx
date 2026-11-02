"use client";

import { useEffect } from "react";

/**
 * Captures the `?gateway=<url>` param that `sectr dev` appends:
 * persists it to localStorage (so navigations keep the gateway without
 * the param) and strips it from the address bar (a refresh or shared link
 * must not re-pin an old port). api.ts reads localStorage on every module
 * init, so this must run before any data fetch — mounted in the root
 * layout. `?gateway=` (empty value) is the escape hatch: clears the pin.
 */
export function GatewayInit() {
    useEffect(() => {
        const param = new URLSearchParams(window.location.search).get(
            "gateway",
        );
        if (param === null) return;
        if (param === "") {
            window.localStorage.removeItem("sectr.gateway");
        } else if (/^https?:\/\//.test(param)) {
            window.localStorage.setItem("sectr.gateway", param);
        } else {
            return; // not a URL — ignore rather than persist junk
        }
        const url = new URL(window.location.href);
        url.searchParams.delete("gateway");
        window.history.replaceState(null, "", url);
    }, []);
    return null;
}
