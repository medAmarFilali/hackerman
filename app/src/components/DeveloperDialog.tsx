import {
  Box,
  Cpu,
  HardDrive,
  Network,
  RefreshCw,
  Wrench,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useState } from "react";

import { CleanupDialog } from "@/components/CleanupDialog";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useAppearance } from "@/lib/appearance";
import { CACHES, FOLDER_KINDS, PROGRAM_KINDS, formatSize } from "@/lib/dev";
import type {
  DevReply,
  DevRequest,
  DiskReport,
  Fingerprint,
  Program,
  UiState,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { askDeveloper } from "@/store";

type TabId = "programs" | "ports" | "docker" | "disk" | "versions";

const TABS: { id: TabId; label: string; icon: LucideIcon }[] = [
  { id: "programs", label: "Memory", icon: Cpu },
  { id: "ports", label: "Ports", icon: Network },
  { id: "docker", label: "Docker", icon: Box },
  { id: "disk", label: "Disk & cleanup", icon: HardDrive },
  { id: "versions", label: "Versions", icon: Wrench },
];

/** A PC that can be chosen, or why it can't. */
interface Pc {
  device: Fingerprint;
  name: string;
  why: string | null;
}

function pcsOf(state: UiState): Pc[] {
  const others = state.devices
    .filter((device) => device.status !== "available")
    .map((device) => ({
      device: device.fingerprint,
      name: device.name,
      why:
        device.status !== "connected"
          ? "not connected"
          : device.hasDeveloperMode
            ? null
            : "needs Hackerman 0.12.0",
    }));
  return [{ device: state.me.fingerprint, name: "This PC", why: null }, ...others];
}

/** Opens developer mode, once it's turned on in Settings. */
export function DeveloperButton({ state }: { state: UiState }) {
  const { developerMode } = useAppearance();
  const [open, setOpen] = useState(false);
  if (!developerMode) return null;
  return (
    <>
      <Tooltip>
        <TooltipTrigger asChild>
          <Button size="icon-sm" variant="ghost" aria-label="Developer" onClick={() => setOpen(true)}>
            <Wrench />
          </Button>
        </TooltipTrigger>
        <TooltipContent>Developer</TooltipContent>
      </Tooltip>
      <DeveloperDialog state={state} open={open} onOpenChange={setOpen} />
    </>
  );
}

function DeveloperDialog({
  state,
  open,
  onOpenChange,
}: {
  state: UiState;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [tab, setTab] = useState<TabId>("programs");
  const [device, setDevice] = useState<Fingerprint>(state.me.fingerprint);
  const pcs = pcsOf(state);
  const pc = pcs.find((candidate) => candidate.device === device) ?? pcs[0];
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex h-[min(44rem,calc(100vh-2rem))] gap-0 overflow-hidden p-0 sm:max-w-4xl">
        <nav aria-label="Developer sections" className="flex w-48 shrink-0 flex-col gap-0.5 border-r bg-muted/40 p-3">
          <DialogTitle className="px-2 pt-1 pb-3">Developer</DialogTitle>
          <DialogDescription className="sr-only">
            What's running on each PC, its ports, Docker and disk space, and cleaning up.
          </DialogDescription>
          {TABS.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              type="button"
              aria-current={tab === id ? "page" : undefined}
              onClick={() => setTab(id)}
              className={cn(
                "flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm outline-none transition-colors",
                "hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50",
                tab === id
                  ? "bg-secondary font-medium text-secondary-foreground hover:bg-secondary"
                  : "text-muted-foreground",
              )}
            >
              <Icon aria-hidden className="size-4 shrink-0" strokeWidth={1.5} />
              <span className="truncate">{label}</span>
            </button>
          ))}
        </nav>

        <div className="flex min-w-0 flex-1 flex-col">
          <div className="flex items-center gap-3 border-b px-5 py-2.5 pe-12">
            <span className="eyebrow">PC</span>
            <Select value={pc.device} onValueChange={setDevice}>
              <SelectTrigger size="sm" aria-label="Which PC" className="w-56">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {pcs.map((candidate) => (
                  <SelectItem key={candidate.device} value={candidate.device} disabled={candidate.why !== null}>
                    {candidate.name}
                    {candidate.why && <span className="text-muted-foreground"> · {candidate.why}</span>}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          {open && (
            <ScrollArea className="min-h-0 flex-1">
              <div className="flex flex-col gap-4 px-5 py-4">
                {tab === "programs" && <ProgramsTab key={pc.device} device={pc.device} />}
                {tab === "ports" && <PortsTab key={pc.device} device={pc.device} />}
                {tab === "docker" && <DockerTab key={pc.device} device={pc.device} />}
                {tab === "disk" && <DiskTab key={pc.device} device={pc.device} pcName={pc.name} />}
                {tab === "versions" && <VersionsTab key={pc.device} device={pc.device} />}
              </div>
            </ScrollArea>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

interface Asked {
  reply: DevReply | null;
  error: string | null;
  loading: boolean;
  refresh: () => void;
}

/** Asks a PC something, again whenever `refresh` is called. */
function useAsk(device: Fingerprint, request: DevRequest): Asked {
  const [answer, setAnswer] = useState<Omit<Asked, "refresh">>({
    reply: null,
    error: null,
    loading: true,
  });
  const [round, setRound] = useState(0);
  const key = JSON.stringify(request);
  useEffect(() => {
    let current = true;
    setAnswer((before) => ({ ...before, loading: true, error: null }));
    askDeveloper(device, JSON.parse(key) as DevRequest)
      .then((reply) => current && setAnswer({ reply, error: null, loading: false }))
      .catch((problem) => current && setAnswer({ reply: null, error: String(problem), loading: false }));
    return () => {
      current = false;
    };
  }, [device, key, round]);
  return { ...answer, refresh: () => setRound((value) => value + 1) };
}

/** Does something on a PC, saying how it went. */
function useAct(device: Fingerprint, then: () => void) {
  const [outcome, setOutcome] = useState<{ text: string; failed: boolean } | null>(null);
  const [busy, setBusy] = useState(false);
  async function act(request: DevRequest) {
    setBusy(true);
    setOutcome(null);
    try {
      const reply = await askDeveloper(device, request);
      if ("done" in reply) setOutcome({ text: reply.done.summary, failed: false });
      else if ("refused" in reply) setOutcome({ text: reply.refused.reason, failed: true });
    } catch (problem) {
      setOutcome({ text: String(problem), failed: true });
    }
    setBusy(false);
    then();
  }
  return { act, busy, outcome };
}

function Waiting({ asked, what }: { asked: Asked; what: string }) {
  if (asked.error) return <Problem text={`No answer: ${asked.error}`} />;
  if (asked.reply && "refused" in asked.reply) return <Problem text={asked.reply.refused.reason} />;
  if (asked.loading && !asked.reply) {
    return <p className="text-sm text-muted-foreground">{what}</p>;
  }
  return null;
}

function Problem({ text }: { text: string }) {
  return (
    <p role="alert" className="text-sm text-amber-700 dark:text-amber-400">
      {text}
    </p>
  );
}

function Outcome({ outcome }: { outcome: { text: string; failed: boolean } | null }) {
  if (!outcome) return null;
  return (
    <p role="status" className={cn("text-sm", outcome.failed ? "text-amber-700 dark:text-amber-400" : "text-signal")}>
      {outcome.text}
    </p>
  );
}

function Heading({
  title,
  detail,
  onRefresh,
  loading,
  children,
}: {
  title: string;
  detail?: string;
  onRefresh: () => void;
  loading: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="flex min-w-0 flex-col gap-0.5">
        <h3 className="text-sm font-medium">{title}</h3>
        {detail && <p className="text-xs text-muted-foreground">{detail}</p>}
      </div>
      <div className="flex shrink-0 items-center gap-2">
        {children}
        <Button size="xs" variant="ghost" onClick={onRefresh} disabled={loading} aria-label="Look again">
          <RefreshCw className={cn(loading && "animate-spin")} />
          Look again
        </Button>
      </div>
    </div>
  );
}

/** A button that asks once more before doing something that can't be undone. */
function ConfirmButton({
  label,
  question,
  onConfirm,
  disabled,
}: {
  label: string;
  question: string;
  onConfirm: () => void;
  disabled?: boolean;
}) {
  const [asking, setAsking] = useState(false);
  if (asking) {
    return (
      <span className="flex items-center gap-1">
        <span className="text-xs text-muted-foreground">{question}</span>
        <Button
          size="xs"
          variant="destructive"
          onClick={() => {
            setAsking(false);
            onConfirm();
          }}
        >
          Yes
        </Button>
        <Button size="xs" variant="ghost" onClick={() => setAsking(false)}>
          No
        </Button>
      </span>
    );
  }
  return (
    <Button size="xs" variant="secondary" disabled={disabled} onClick={() => setAsking(true)}>
      {label}
    </Button>
  );
}

function programName(program: Program): string {
  return program.project ? `${program.name} (${program.project})` : program.name;
}

function ProgramsTab({ device }: { device: Fingerprint }) {
  const asked = useAsk(device, "programs");
  const { act, busy, outcome } = useAct(device, asked.refresh);
  const [toolsOnly, setToolsOnly] = useState(false);
  const report = asked.reply && "programs" in asked.reply ? asked.reply.programs : null;
  const programs = (report?.programs ?? [])
    .filter((program) => !toolsOnly || program.kind !== "other")
    .slice(0, 40);
  const used = report ? report.memoryUsed / Math.max(report.memoryTotal, 1) : 0;
  return (
    <>
      <Heading
        title="What's using memory"
        detail="Programs with every process they started, most memory first. Stopping one ends all of them, without saving anything."
        onRefresh={asked.refresh}
        loading={asked.loading}
      >
        <Button size="xs" variant={toolsOnly ? "secondary" : "ghost"} aria-pressed={toolsOnly} onClick={() => setToolsOnly(!toolsOnly)}>
          Developer tools only
        </Button>
      </Heading>
      <Waiting asked={asked} what="Looking at what's running…" />
      <Outcome outcome={outcome} />
      {report && (
        <>
          <div className="flex flex-col gap-1">
            <div className="flex justify-between text-xs text-muted-foreground tabular-nums">
              <span>Memory in use</span>
              <span>
                {formatSize(report.memoryUsed)} of {formatSize(report.memoryTotal)}
              </span>
            </div>
            <div className="h-1.5 overflow-hidden rounded-full bg-muted">
              <div
                className={cn("h-full rounded-full", used > 0.9 ? "bg-destructive" : "bg-signal")}
                style={{ width: `${Math.round(used * 100)}%` }}
              />
            </div>
          </div>
          <ul className="flex flex-col divide-y rounded-md border">
            {programs.map((program) => (
              <li key={`${program.id.pid}-${program.id.started}`} className="flex items-center gap-3 px-3 py-2">
                <div className="flex min-w-0 flex-1 flex-col">
                  <span className="truncate text-sm font-medium">{programName(program)}</span>
                  <span className="text-xs text-muted-foreground">
                    {PROGRAM_KINDS[program.kind]} · {program.processes}{" "}
                    {program.processes === 1 ? "process" : "processes"}
                  </span>
                </div>
                <span className="shrink-0 text-sm tabular-nums">{formatSize(program.memory)}</span>
                <ConfirmButton
                  label="Stop"
                  question={program.processes > 1 ? `Stop all ${program.processes}?` : "Stop it?"}
                  disabled={!program.canStop || busy}
                  onConfirm={() => void act({ stop: { program: program.id } })}
                />
              </li>
            ))}
          </ul>

          <div className="flex items-start justify-between gap-4 pt-2">
            <div className="flex min-w-0 flex-col gap-0.5">
              <h3 className="text-sm font-medium">Build helpers left running</h3>
              <p className="text-xs text-muted-foreground">
                .NET, esbuild, Turborepo and Gradle keep these going after a build, to start the
                next one sooner. Stopping them is safe: the next build starts them again.
              </p>
            </div>
            <ConfirmButton
              label="Stop them all"
              question="Stop every build helper?"
              disabled={report.buildHelpers.length === 0 || busy}
              onConfirm={() => void act("stopBuildHelpers")}
            />
          </div>
          {report.buildHelpers.length === 0 ? (
            <p className="text-xs text-muted-foreground">None are running.</p>
          ) : (
            <ul className="flex flex-col divide-y rounded-md border">
              {report.buildHelpers.map((helper) => (
                <li key={`${helper.id.pid}-${helper.id.started}`} className="flex items-center justify-between gap-3 px-3 py-1.5 text-sm">
                  <span className="truncate">{helper.name}</span>
                  <span className="tabular-nums text-muted-foreground">{formatSize(helper.memory)}</span>
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </>
  );
}

function PortsTab({ device }: { device: Fingerprint }) {
  const asked = useAsk(device, "programs");
  const { act, busy, outcome } = useAct(device, asked.refresh);
  const [everything, setEverything] = useState(false);
  const report = asked.reply && "programs" in asked.reply ? asked.reply.programs : null;
  // Windows' own services listen on plenty of ports nobody needs to see.
  const ports = (report?.ports ?? []).filter((port) => everything || port.name !== "Windows");
  return (
    <>
      <Heading
        title="Ports in use"
        detail="What's waiting for connections on each port, like a dev server on 3000. Freeing a port stops what's using it."
        onRefresh={asked.refresh}
        loading={asked.loading}
      >
        <Button size="xs" variant={everything ? "secondary" : "ghost"} aria-pressed={everything} onClick={() => setEverything(!everything)}>
          Windows' own too
        </Button>
      </Heading>
      <Waiting asked={asked} what="Looking at the ports…" />
      <Outcome outcome={outcome} />
      {report && (
        <ul className="flex flex-col divide-y rounded-md border">
          {ports.map((port) => (
            <li key={`${port.port}-${port.address}`} className="flex items-center gap-3 px-3 py-1.5">
              <span className="w-14 shrink-0 font-mono text-sm tabular-nums">{port.port}</span>
              <span className="w-32 shrink-0 truncate font-mono text-xs text-muted-foreground" title={port.address}>
                {port.address}
              </span>
              <span className="min-w-0 flex-1 truncate text-sm">{port.name}</span>
              {port.container ? (
                <ConfirmButton
                  label="Stop container"
                  question={`Stop ${port.container}?`}
                  disabled={busy}
                  onConfirm={() =>
                    void act({ dockerContainer: { id: port.container!, action: "stop" } })
                  }
                />
              ) : (
                <ConfirmButton
                  label="Free it"
                  question={`Stop ${port.name}?`}
                  disabled={!port.canStop || !port.program || busy}
                  onConfirm={() => port.program && void act({ stop: { program: port.program } })}
                />
              )}
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

function DockerTab({ device }: { device: Fingerprint }) {
  const asked = useAsk(device, "docker");
  const { act, busy, outcome } = useAct(device, asked.refresh);
  const report = asked.reply && "docker" in asked.reply ? asked.reply.docker : null;
  return (
    <>
      <Heading
        title="Docker"
        detail="Containers with what they're using, and the file Docker keeps its disk in."
        onRefresh={asked.refresh}
        loading={asked.loading}
      >
        {report?.running && (
          <ConfirmButton
            label="Stop all containers"
            question="Stop every running container?"
            disabled={busy || !report.containers.some((container) => container.running)}
            onConfirm={() => void act("stopAllContainers")}
          />
        )}
      </Heading>
      <Waiting asked={asked} what="Asking Docker…" />
      <Outcome outcome={outcome} />
      {report && !report.installed && <p className="text-sm text-muted-foreground">Docker isn't installed.</p>}
      {report && report.installed && !report.running && (
        <p className="text-sm text-muted-foreground">Docker isn't running.</p>
      )}
      {report?.running && (
        <ul className="flex flex-col divide-y rounded-md border">
          {report.containers.map((container) => (
            <li key={container.id} className="flex items-center gap-3 px-3 py-2">
              <span className={cn("size-2 shrink-0 rounded-full", container.running ? "bg-signal" : "bg-muted-foreground/40")} />
              <div className="flex min-w-0 flex-1 flex-col">
                <span className="truncate text-sm font-medium">{container.name}</span>
                <span className="truncate text-xs text-muted-foreground">
                  {container.image} · {container.status}
                </span>
              </div>
              <span className="shrink-0 text-xs tabular-nums text-muted-foreground">
                {container.memory !== null && formatSize(container.memory)}
                {container.cpu !== null && ` · ${container.cpu.toFixed(1)}%`}
              </span>
              {container.running && (
                <>
                  <ConfirmButton
                    label="Restart"
                    question="Restart it?"
                    disabled={busy}
                    onConfirm={() => void act({ dockerContainer: { id: container.id, action: "restart" } })}
                  />
                  <ConfirmButton
                    label="Stop"
                    question="Stop it?"
                    disabled={busy}
                    onConfirm={() => void act({ dockerContainer: { id: container.id, action: "stop" } })}
                  />
                </>
              )}
              {!container.running && (
                <Button
                  size="xs"
                  variant="secondary"
                  disabled={busy}
                  onClick={() => void act({ dockerContainer: { id: container.id, action: "start" } })}
                >
                  Start
                </Button>
              )}
            </li>
          ))}
        </ul>
      )}
      {report?.diskImage && <DockerDisk device={device} size={report.diskImage.size} busy={busy} act={act} />}
    </>
  );
}

/** Shrinking the file Docker keeps its disk in, which never gets smaller by itself. */
function DockerDisk({
  size,
  busy,
  act,
}: {
  device: Fingerprint;
  size: number;
  busy: boolean;
  act: (request: DevRequest) => Promise<void>;
}) {
  return (
    <div className="flex flex-col gap-2 rounded-md border px-3 py-2.5">
      <div className="flex items-start justify-between gap-4">
        <div className="flex min-w-0 flex-col gap-0.5">
          <span className="text-sm font-medium">Docker's disk file: {formatSize(size)}</span>
          <span className="text-xs text-muted-foreground">
            Cleaning up inside Docker frees space in this file, but the file itself stays as big
            until it's shrunk. To shrink it, quit Docker Desktop first. Shrinking stops every WSL
            Linux on that PC, and Windows asks for administrator permission there, so someone has
            to be at that PC to say yes. It can take several minutes.
          </span>
        </div>
        <ConfirmButton
          label="Shrink it"
          question="Stop WSL and shrink it?"
          disabled={busy}
          onConfirm={() => void act("compactDockerDisk")}
        />
      </div>
    </div>
  );
}

function DiskTab({ device, pcName }: { device: Fingerprint; pcName: string }) {
  const asked = useAsk(device, "disk");
  const [cleaning, setCleaning] = useState(false);
  const report: DiskReport | null = asked.reply && "disk" in asked.reply ? asked.reply.disk : null;
  return (
    <>
      <Heading
        title="Disk"
        detail="The folders builds and installs leave in your projects, and the caches of downloaded packages."
        onRefresh={asked.refresh}
        loading={asked.loading}
      >
        <Button size="xs" variant="default" disabled={!report} onClick={() => setCleaning(true)}>
          Clean up…
        </Button>
      </Heading>
      <Waiting asked={asked} what="Looking through your projects. Measuring large folders can take a minute…" />
      {report && (
        <>
          <div className="flex flex-wrap gap-2">
            {report.drives.map((drive) => (
              <div key={drive.name} className="flex flex-col gap-1 rounded-md border px-3 py-2 text-xs">
                <span className="font-medium">{drive.name}</span>
                <span className="tabular-nums text-muted-foreground">
                  {formatSize(drive.free)} free of {formatSize(drive.total)}
                </span>
              </div>
            ))}
          </div>
          <p className="text-xs text-muted-foreground">
            Looked in {report.roots.length === 0 ? "nowhere: none of the usual folders are there. Choose some in Settings, under Developer." : report.roots.join(", ")}
          </p>
          <ul className="flex flex-col divide-y rounded-md border">
            {report.folders.slice(0, 50).map((folder) => (
              <li key={folder.path} className="flex items-center gap-3 px-3 py-1.5">
                <div className="flex min-w-0 flex-1 flex-col">
                  <span className="truncate text-sm">
                    {folder.project}: {FOLDER_KINDS[folder.kind]}
                    {folder.inUse && <span className="text-xs text-muted-foreground"> · in use</span>}
                  </span>
                  <span className="truncate text-xs text-muted-foreground" title={folder.path}>
                    {folder.path}
                  </span>
                </div>
                <span className="shrink-0 text-sm tabular-nums">{formatSize(folder.size)}</span>
              </li>
            ))}
          </ul>
          <ul className="flex flex-col divide-y rounded-md border">
            {report.caches.map((cache) => (
              <li key={cache.kind} className="flex items-center justify-between gap-3 px-3 py-1.5">
                <div className="flex min-w-0 flex-col">
                  <span className="truncate text-sm">{CACHES[cache.kind].name}</span>
                  <span className="truncate text-xs text-muted-foreground" title={cache.path}>
                    {cache.path}
                  </span>
                </div>
                <span className="shrink-0 text-sm tabular-nums">{formatSize(cache.size)}</span>
              </li>
            ))}
            {report.dockerImage && (
              <li className="flex items-center justify-between gap-3 px-3 py-1.5 text-sm">
                <span className="truncate">Docker's disk file</span>
                <span className="shrink-0 tabular-nums">{formatSize(report.dockerImage.size)}</span>
              </li>
            )}
          </ul>
          <CleanupDialog
            device={device}
            pcName={pcName}
            disk={report}
            open={cleaning}
            onOpenChange={(open) => {
              setCleaning(open);
              if (!open) asked.refresh();
            }}
          />
        </>
      )}
    </>
  );
}

function VersionsTab({ device }: { device: Fingerprint }) {
  const asked = useAsk(device, "versions");
  const tools = asked.reply && "versions" in asked.reply ? asked.reply.versions.tools : null;
  return (
    <>
      <Heading title="Installed versions" onRefresh={asked.refresh} loading={asked.loading} />
      <Waiting asked={asked} what="Asking each tool…" />
      {tools && (
        <ul className="flex flex-col divide-y rounded-md border">
          {tools.map((tool) => (
            <li key={tool.tool} className="flex items-center justify-between gap-3 px-3 py-1.5 text-sm">
              <span>{tool.tool}</span>
              <span className={cn("font-mono text-xs", !tool.version && "text-muted-foreground")}>
                {tool.version ?? "Not installed"}
              </span>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
