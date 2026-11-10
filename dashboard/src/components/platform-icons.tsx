"use client";

import { useId } from "react";
import { Laptop, Server } from "lucide-react";
import { LogoMark } from "@/components/logo";
import { platformOf, type RunsOnKind } from "@/lib/connections";

/** The marks of the platforms an endpoint runs on, in their own colors.
 *  From the Iconify "logos" set (CC0-1.0, @iconify-json/logos 1.2.19); the
 *  trademarks belong to Amazon, Google and Docker. */

/** AWS Lambda's architecture icon: the λ on its orange tile. */
export function LambdaIcon({ className }: { className?: string }) {
  // A gradient id per instance: two icons on a page can't share one.
  const id = useId();
  return (
    <svg viewBox="0 0 256 256" aria-hidden="true" className={className}>
      <defs>
        <linearGradient id={id} x1="0%" x2="100%" y1="100%" y2="0%">
          <stop offset="0%" stopColor="#c8511b" />
          <stop offset="100%" stopColor="#f90" />
        </linearGradient>
      </defs>
      <rect width="256" height="256" rx="24" fill={`url(#${id})`} />
      <path
        fill="#fff"
        d="M89.624 211.2H49.89l43.945-91.853l19.912 40.992zm7.079-100.63a3.22 3.22 0 0 0-2.887-1.805h-.01a3.2 3.2 0 0 0-2.886 1.82L41.913 213.022a3.203 3.203 0 0 0 2.893 4.58l46.848-.001a3.21 3.21 0 0 0 2.9-1.83l25.65-54.08a3.18 3.18 0 0 0-.016-2.762zM207.985 211.2h-39.477L105.174 78.624a3.21 3.21 0 0 0-2.897-1.824h-25.83l.03-32h50.626l63.042 132.573a3.21 3.21 0 0 0 2.897 1.827h14.943zm3.208-38.4h-16.121L132.03 40.227a3.21 3.21 0 0 0-2.9-1.827H73.273a3.206 3.206 0 0 0-3.208 3.197l-.035 38.4c0 .851.333 1.664.94 2.265c.6.602 1.414.938 2.267.938h27.017l63.337 132.576a3.2 3.2 0 0 0 2.893 1.824h44.709a3.203 3.203 0 0 0 3.207-3.2V176c0-1.766-1.434-3.2-3.207-3.2"
      />
    </svg>
  );
}

/** Google Cloud Run's product icon: the blue hexagon. */
export function CloudRunIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 256 231" aria-hidden="true" className={className}>
      <path
        fill="#4285f4"
        d="M75.39 0a23.1 23.1 0 0 0-19.72 11.76L3.054 102.995a22.96 22.96 0 0 0 0 22.879l52.598 91.997a23.54 23.54 0 0 0 19.719 12.18h105.216a23.46 23.46 0 0 0 19.74-12.12l52.598-91.477a23.44 23.44 0 0 0 3.074-11.61c0-4.004-1.025-8.01-3.074-11.61L200.326 11.76A23.12 23.12 0 0 0 180.606 0z"
      />
      <path
        fillOpacity=".07"
        d="m236.495 155.027l-36.17 62.905a23.46 23.46 0 0 1-19.739 12.12H140.51l-58.472-58.995l17.238-55.583l-17.238-56.22l17.705 11.893l23.806 24.019l-11.12-35.912l83.375 56.008z"
      />
      <path
        fill="#fff"
        d="m82.038 59.254l17.705 11.893l13.88 44.618l-13.633 43.278l-17.952 12.014l17.343-55.796zm45.347 19.836l8.593 27.792h32.989zm68.42 36.172l-83.376 55.795l17.343-55.795l-17.343-56.008z"
      />
    </svg>
  );
}

/** Any other container platform (Fly, ECS, Kubernetes, …): Docker's whale,
 *  since what runs there is the image `sectr init --endpoint` set up. */
export function ContainerIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 256 185" aria-hidden="true" className={className}>
      <path
        fill="#2396ed"
        d="M250.716 70.497c-5.765-4-18.976-5.5-29.304-3.5c-1.2-10-6.725-18.749-16.333-26.499l-5.524-4l-3.844 5.75c-4.803 7.5-7.205 18-6.485 28c.24 3.499 1.441 9.749 5.044 15.249c-3.362 2-10.328 4.5-19.455 4.5H1.155l-.48 2c-1.682 9.999-1.682 41.248 18.014 65.247c14.892 18.249 36.99 27.499 66.053 27.499c62.93 0 109.528-30.25 131.386-84.997c8.647.25 27.142 0 36.51-18.75c.24-.5.72-1.5 2.401-5.249l.961-2zM139.986 0h-26.42v24.999h26.42zm0 29.999h-26.42v24.999h26.42zm-31.225 0h-26.42v24.999h26.42zm-31.225 0H51.115v24.999h26.421zM46.311 59.998H19.89v24.999h26.42zm31.225 0H51.115v24.999h26.421zm31.225 0h-26.42v24.999h26.42zm31.226 0h-26.422v24.999h26.422zm31.225 0H144.79v24.999h26.422z"
      />
    </svg>
  );
}

/** The mark for an endpoint URL's platform. */
export function EndpointPlatformIcon({ url, className }: { url: string; className?: string }) {
  switch (platformOf(url)) {
    case "lambda":
      return <LambdaIcon className={className} />;
    case "cloud-run":
      return <CloudRunIcon className={className} />;
    default:
      return <ContainerIcon className={className} />;
  }
}

/** The bare mark for where a project runs: the platform's own, Sectr's
 *  logo for Sectr Run, a lucide icon for the rest. `className` sizes it. */
export function RunsOnMark({ kind, className }: { kind: RunsOnKind; className?: string }) {
  switch (kind) {
    case "lambda":
      return <LambdaIcon className={className} />;
    case "cloud-run":
      return <CloudRunIcon className={className} />;
    case "endpoint":
      return <ContainerIcon className={className} />;
    case "sectr":
      return <LogoMark className={className} />;
    case "local":
      return <Laptop className={className} />;
    case "none":
      return <Server className={className} />;
  }
}
