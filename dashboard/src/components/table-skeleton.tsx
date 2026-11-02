import { Skeleton } from "@/components/ui/skeleton";
import { TableCell, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";

/** Placeholder rows while a table loads: one bar per column, with the given
 *  Tailwind width classes (an empty string leaves that column blank). */
export function TableSkeletonRows({
  widths,
  rows = 3,
  firstCellClassName,
}: {
  widths: string[];
  rows?: number;
  firstCellClassName?: string;
}) {
  return Array.from({ length: rows }, (_, r) => (
    <TableRow key={r} className="hover:bg-transparent" aria-hidden>
      {widths.map((w, c) => (
        <TableCell key={c} className={cn(c === 0 && firstCellClassName)}>
          {w && <Skeleton className={cn("h-3.5", w)} />}
        </TableCell>
      ))}
    </TableRow>
  ));
}
