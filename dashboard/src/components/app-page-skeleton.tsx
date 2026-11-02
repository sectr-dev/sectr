import { Skeleton } from "@/components/ui/skeleton";
import { Card } from "@/components/ui/card";

/** What a project page looks like while the project loads: the header, the
 *  tab bar and a couple of cards, so the page doesn't pop in from blank. */
export function AppPageSkeleton() {
  return (
    <div className="pb-12" aria-busy aria-label="Loading project">
      <div className="border-b border-border px-6 py-5">
        <Skeleton className="mb-3 h-3 w-40" />
        <div className="flex items-center gap-2">
          <Skeleton className="size-8 rounded-lg" />
          <Skeleton className="h-8 w-56" />
        </div>
        <Skeleton className="mt-3 h-4 w-96 max-w-full" />
      </div>
      <div className="flex gap-1 border-b border-border px-6">
        {[16, 20, 20, 16].map((w, i) => (
          <div key={i} className="flex h-9 items-center px-3">
            <Skeleton className="h-3.5" style={{ width: `${w * 4}px` }} />
          </div>
        ))}
      </div>
      <div className="grid gap-4 px-6 pt-6 lg:grid-cols-3">
        <Card className="lg:col-span-1">
          <div className="space-y-3 px-4">
            <Skeleton className="h-5 w-24" />
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </div>
        </Card>
        <Card className="lg:col-span-2">
          <div className="space-y-3 px-4">
            <Skeleton className="h-5 w-32" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
            <Skeleton className="h-8 w-full" />
          </div>
        </Card>
      </div>
    </div>
  );
}
