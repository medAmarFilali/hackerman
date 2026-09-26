import { CircleAlert, Loader2, Lock, LockOpen, MonitorUp, Power } from "lucide-react";

import { PairingArt } from "@/components/Art";
import { DevicesPanel } from "@/components/DevicesPanel";
import { IncomingFilesDialog } from "@/components/IncomingFilesDialog";
import { CanvasGuide, MonitorCanvas } from "@/components/MonitorCanvas";
import { PairingDialog } from "@/components/PairingDialog";
import { SettingsDialog } from "@/components/SettingsDialog";
import { ThemeSwitch } from "@/components/ThemeSwitch";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { headline } from "@/lib/devices";
import type { UiState } from "@/lib/types";
import { cn } from "@/lib/utils";
import { send, useApp } from "@/store";

export function App() {
  const state = useApp((store) => store.state);
  const agentProblem = useApp((store) => store.agentProblem);

  if (!state) {
    return <Starting problem={agentProblem} />;
  }
  return (
    <TooltipProvider>
      <div className="grid h-screen grid-cols-[minmax(0,1fr)_20rem] grid-rows-[auto_auto_auto_minmax(0,1fr)] bg-background text-foreground">
        <TopBar state={state} />
        <div className="col-span-2">
          {agentProblem && <Banner>{agentProblem}</Banner>}
          {state.captureError && (
            <Banner>
              This PC can't send its keyboard and mouse to other PCs: {state.captureError}
            </Banner>
          )}
        </div>
        <Status state={state} />
        <main className="flex min-h-0 flex-col">
          <MonitorCanvas state={state} />
          <CanvasGuide />
        </main>
        <DevicesPanel state={state} />
      </div>
      <PairingDialog pairing={state.pairing} />
      <IncomingFilesDialog state={state} />
    </TooltipProvider>
  );
}

function Starting({ problem }: { problem: string | null }) {
  return (
    <div className="flex h-screen flex-col items-center justify-center gap-5 bg-background p-8 text-center text-foreground">
      <PairingArt live={false} />
      {problem ? (
        <p className="flex max-w-md items-start gap-2 text-sm">
          <CircleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-destructive" />
          {problem}
        </p>
      ) : (
        <p className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 aria-hidden className="size-4 animate-spin" />
          Starting Hackerman…
        </p>
      )}
    </div>
  );
}

function Banner({ children }: { children: React.ReactNode }) {
  return (
    <p className="flex items-start gap-2 border-b bg-card px-5 py-2 text-sm">
      <CircleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-destructive" />
      <span>{children}</span>
    </p>
  );
}

function TopBar({ state }: { state: UiState }) {
  return (
    <header className="col-span-2 flex h-11 items-center justify-between gap-4 border-b bg-card px-5">
      <h1 className="flex items-center gap-2 text-sm font-medium">
        <MonitorUp aria-hidden className="size-4" strokeWidth={1.5} />
        Hackerman
        <span className="eyebrow tracking-normal normal-case tabular-nums">{state.me.version}</span>
      </h1>
      <div className="flex items-center gap-2">
        <ThemeSwitch />
        <SettingsDialog state={state} />
      </div>
    </header>
  );
}

/** What's going on right now, and the two buttons that change it. */
function Status({ state }: { state: UiState }) {
  const { title, detail } = headline(state);
  return (
    <section className="col-span-2 flex items-end justify-between gap-6 border-b px-5 pt-3.5 pb-4 [@media(max-height:640px)]:py-2.5">
      <div role="status" className="min-w-0">
        <p
          className={cn(
            "inline-flex items-center gap-1.5 rounded-full border bg-card px-2 py-0.5 text-[0.6875rem] font-medium [@media(max-height:640px)]:hidden",
            !state.enabled && "text-muted-foreground",
          )}
        >
          <span
            aria-hidden
            className={cn(
              "size-1.5 rounded-full",
              state.enabled ? "bg-signal" : "border border-muted-foreground",
            )}
          />
          {state.enabled ? "Sharing on" : "Sharing off"}
        </p>
        <h2 className="mt-2 truncate text-[1.625rem] leading-tight font-medium tracking-tight [@media(max-height:640px)]:mt-0 [@media(max-height:640px)]:text-lg">
          {title}
        </h2>
        <p className="mt-0.5 truncate text-sm text-muted-foreground [@media(max-height:640px)]:hidden">
          {detail}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <LockButton state={state} />
        <PowerButton enabled={state.enabled} />
      </div>
    </section>
  );
}

function PowerButton({ enabled }: { enabled: boolean }) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button size="lg" onClick={() => send({ type: "setEnabled", enabled: !enabled })}>
          <Power />
          {enabled ? "Disable" : "Enable"}
        </Button>
      </TooltipTrigger>
      <TooltipContent className="max-w-64">
        {enabled
          ? "Stop sharing: this PC keeps its keyboard and mouse, and other PCs can't reach it."
          : "Start sharing this PC's keyboard, mouse and clipboard again."}
      </TooltipContent>
    </Tooltip>
  );
}

function LockButton({ state }: { state: UiState }) {
  const { locked, enabled } = state;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        {/* The span keeps the tooltip working while the button is disabled. */}
        <span tabIndex={enabled ? undefined : 0}>
          <Button
            size="lg"
            variant="outline"
            disabled={!enabled}
            aria-pressed={locked}
            onClick={() => send({ type: "setLocked", locked: !locked })}
          >
            {locked ? <Lock /> : <LockOpen />}
            {locked ? "Locked" : "Lock"}
          </Button>
        </span>
      </TooltipTrigger>
      <TooltipContent>
        {!enabled
          ? "There's nothing to lock while Hackerman is disabled"
          : locked
            ? "The cursor is staying on the PC it's on. Click to let it cross again."
            : "Keep the cursor on the PC it's on now"}
        {enabled && state.rules.shortcuts && " (Ctrl+Alt+L)"}
      </TooltipContent>
    </Tooltip>
  );
}
