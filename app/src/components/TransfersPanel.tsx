import { ArrowDown, ArrowUp, Copy, FolderOpen, X } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatBytes, transferProgress } from "@/lib/format";
import type { TransferView, UiState } from "@/lib/types";
import { cn } from "@/lib/utils";
import { openFolder, send } from "@/store";

function isOver(transfer: TransferView): boolean {
  return ["done", "failed", "cancelled"].includes(transfer.stage.stage);
}

/** What's happening with it, in words. */
function statusOf(transfer: TransferView, peerName: string, onThisPc: boolean): string {
  const { stage } = transfer;
  switch (stage.stage) {
    case "waiting":
      return transfer.sending && !onThisPc ? `Waiting for ${peerName} to accept…` : "Starting…";
    case "running":
      return `${formatBytes(transfer.doneBytes)} of ${formatBytes(transfer.totalBytes)}`;
    case "done":
      return transfer.folder ? `Saved in ${transfer.folder}` : "Saved";
    case "failed":
      return stage.reason;
    case "cancelled":
      return "Cancelled";
  }
}

export function TransfersPanel({ state }: { state: UiState }) {
  const nameOf = (device: string) =>
    device === state.me.fingerprint
      ? "this PC"
      : (state.devices.find((d) => d.fingerprint === device)?.name ?? "another PC");
  return (
    <div className="flex flex-col gap-2">
      {[...state.transfers].reverse().map((transfer) => (
        <Transfer
          key={transfer.id}
          transfer={transfer}
          peerName={nameOf(transfer.peer)}
          onThisPc={transfer.peer === state.me.fingerprint}
        />
      ))}
    </div>
  );
}

function Transfer({
  transfer,
  peerName,
  onThisPc,
}: {
  transfer: TransferView;
  peerName: string;
  onThisPc: boolean;
}) {
  const over = isOver(transfer);
  const failed = transfer.stage.stage === "failed";
  const done = transfer.stage.stage === "done";
  const Icon = onThisPc ? Copy : transfer.sending ? ArrowUp : ArrowDown;
  const direction = onThisPc ? "On this PC" : transfer.sending ? `To ${peerName}` : `From ${peerName}`;
  const status = statusOf(transfer, peerName, onThisPc);
  const progress = transferProgress(transfer.doneBytes, transfer.totalBytes);
  const canOpen = done && !transfer.sending && transfer.folder !== null;

  return (
    <div data-transfer={transfer.stage.stage} className="rounded-lg border p-2">
      <div className="flex min-w-0 items-start gap-2">
        <span
          aria-hidden
          className={cn(
            "mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-md",
            failed ? "bg-destructive/10 text-destructive" : "bg-signal-soft text-signal",
          )}
        >
          <Icon className="size-3" strokeWidth={2} />
        </span>
        <div className="min-w-0 flex-1">
          <div title={transfer.label} className="truncate text-xs font-medium">
            {transfer.label}
          </div>
          <div className="truncate text-[0.6875rem] text-muted-foreground">{direction}</div>
        </div>
        {canOpen && (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                size="icon-xs"
                variant="ghost"
                aria-label={`Open the folder for ${transfer.label}`}
                className="-my-0.5 text-muted-foreground"
                onClick={() => openFolder(transfer.folder ?? "")}
              >
                <FolderOpen />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Open the folder</TooltipContent>
          </Tooltip>
        )}
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="icon-xs"
              variant="ghost"
              aria-label={over ? `Dismiss ${transfer.label}` : `Cancel ${transfer.label}`}
              className="-my-0.5 text-muted-foreground"
              onClick={() =>
                send({
                  type: over ? "dismissTransfer" : "cancelTransfer",
                  id: transfer.id,
                })
              }
            >
              <X />
            </Button>
          </TooltipTrigger>
          <TooltipContent>{over ? "Dismiss" : "Cancel"}</TooltipContent>
        </Tooltip>
      </div>
      {!over && (
        <div
          role="progressbar"
          aria-label={`Progress of ${transfer.label}`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={progress}
          className="mt-2 h-1 overflow-hidden rounded-full bg-muted"
        >
          <div
            style={{ width: `${progress}%` }}
            className="h-full rounded-full bg-signal transition-[width] duration-200 motion-reduce:transition-none"
          />
        </div>
      )}
      <p
        title={status}
        className={cn(
          "mt-1.5 truncate text-[0.6875rem] tabular-nums",
          failed ? "text-destructive" : "text-muted-foreground",
        )}
      >
        {status}
      </p>
    </div>
  );
}
