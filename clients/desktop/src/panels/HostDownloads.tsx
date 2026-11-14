// What the Host is downloading (an agent's adapter, the browser engine, a
// Host update), above the composer: "Downloading Claude Code… 23 MB / 89 MB".
// The Host downloads those one at a time, so only the one under way has a
// row; the ones queued behind it read as "· 2 waiting" on it. A file a page
// downloads never waits and has its own row. Rows go when their download ends.

import { Download } from "lucide-react";
import { downloadProgress } from "@zwiim/sdk";
import { useHost } from "@/lib/host-context";

export function HostDownloads() {
  const downloads = useHost((s) => s.downloads);
  if (downloads.length === 0) return null;
  const waiting = downloads.filter((d) => d.waiting);
  const going = downloads.filter((d) => !d.waiting);
  // Between one ending and the next starting, the next is shown waiting.
  const rows = going.length > 0 ? going : waiting.slice(0, 1);
  const behind = waiting.filter((d) => !rows.includes(d));
  return (
    <div className="flex flex-col gap-1.5" role="status" aria-label="Downloads">
      {rows.map((d, i) => (
        <div
          key={d.download_id}
          className="border-border bg-card animate-fade-in flex h-10 items-center gap-2.5 rounded-xl border px-3.5"
        >
          <Download className="text-muted-foreground size-4 shrink-0" aria-hidden />
          <span className="text-ui min-w-0 truncate font-medium">
            {d.waiting ? "Waiting to download" : "Downloading"} {d.label}…
          </span>
          {!d.waiting && (
            <span className="text-muted-foreground text-ui shrink-0 tabular-nums">{downloadProgress(d)}</span>
          )}
          {i === rows.length - 1 && behind.length > 0 && (
            <span
              className="text-muted-foreground text-ui ml-auto shrink-0"
              title={`Next: ${behind.map((b) => b.label).join(", ")}`}
            >
              · {behind.length} waiting
            </span>
          )}
        </div>
      ))}
    </div>
  );
}
