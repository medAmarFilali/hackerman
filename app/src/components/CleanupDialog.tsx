import { AlertTriangle } from "lucide-react";
import { useEffect, useId, useState } from "react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  CONFIRM_WORD,
  type Choice,
  RISKS,
  type Risk,
  cleanupChoices,
  formatSize,
  keyOf,
  recentlyUsed,
  summary,
} from "@/lib/dev";
import type { CleanResult, DiskReport, DockerReport, Fingerprint } from "@/lib/types";
import { cn } from "@/lib/utils";
import { askDeveloper } from "@/store";

type Step = "warning" | "choosing" | "cleaning" | "done";

const KEEP = [
  { value: "0", label: "Don't keep any" },
  { value: "7", label: "Changed in the last 7 days" },
  { value: "30", label: "Changed in the last 30 days" },
  { value: "90", label: "Changed in the last 90 days" },
];

/** Cleaning up a PC: a warning first, every time, then a list to choose from, then what was done. */
export function CleanupDialog({
  device,
  pcName,
  disk,
  open,
  onOpenChange,
}: {
  device: Fingerprint;
  pcName: string;
  disk: DiskReport;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [step, setStep] = useState<Step>("warning");
  const [docker, setDocker] = useState<DockerReport | null>(null);
  const [results, setResults] = useState<CleanResult[]>([]);
  const [cleaned, setCleaned] = useState<Choice[]>([]);
  const [problem, setProblem] = useState<string | null>(null);

  // Every time it's opened, it starts from the warning.
  useEffect(() => {
    if (!open) return;
    setStep("warning");
    setResults([]);
    setProblem(null);
    let current = true;
    askDeveloper(device, "docker")
      .then((reply) => current && "docker" in reply && setDocker(reply.docker))
      .catch(() => current && setDocker(null));
    return () => {
      current = false;
    };
  }, [open, device]);

  async function clean(chosen: Choice[]) {
    setStep("cleaning");
    setCleaned(chosen);
    try {
      const reply = await askDeveloper(device, { clean: { items: chosen.map((choice) => choice.item) } });
      if ("cleaned" in reply) setResults(reply.cleaned.results);
      else if ("refused" in reply) setProblem(reply.refused.reason);
    } catch (failure) {
      setProblem(String(failure));
    }
    setStep("done");
  }

  return (
    <Dialog open={open} onOpenChange={(next) => step !== "cleaning" && onOpenChange(next)}>
      <DialogContent className="flex h-[min(42rem,calc(100vh-2rem))] flex-col gap-0 overflow-hidden p-0 sm:max-w-2xl">
        <div className="border-b px-5 py-3.5 pe-12">
          <DialogTitle>Clean up {pcName}</DialogTitle>
          <DialogDescription className="text-xs">
            {step === "warning" && "Read this first."}
            {step === "choosing" && "Choose what to delete. Nothing is deleted until you confirm."}
            {step === "cleaning" && "Deleting. Large folders can take several minutes."}
            {step === "done" && "Done. Here's what happened."}
          </DialogDescription>
        </div>
        {step === "warning" && <Warning pcName={pcName} onCancel={() => onOpenChange(false)} onContinue={() => setStep("choosing")} />}
        {step === "choosing" && (
          <Choosing
            choices={cleanupChoices(disk, docker)}
            dockerKnown={docker !== null}
            onCancel={() => onOpenChange(false)}
            onClean={(chosen) => void clean(chosen)}
          />
        )}
        {step === "cleaning" && (
          <p className="px-5 py-6 text-sm text-muted-foreground">
            Deleting on {pcName}. Keep Hackerman open until it's done.
          </p>
        )}
        {step === "done" && (
          <Results results={results} chosen={cleaned} problem={problem} onClose={() => onOpenChange(false)} />
        )}
      </DialogContent>
    </Dialog>
  );
}

function Warning({
  pcName,
  onCancel,
  onContinue,
}: {
  pcName: string;
  onCancel: () => void;
  onContinue: () => void;
}) {
  const id = useId();
  const [understood, setUnderstood] = useState(false);
  return (
    <>
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex flex-col gap-3 px-5 py-4 text-sm">
          <p className="flex items-center gap-2 font-medium text-destructive">
            <AlertTriangle aria-hidden className="size-4 shrink-0" />
            Deleting can't be undone
          </p>
          <p>
            Hackerman deletes what you choose on {pcName} for good. It doesn't go to the Recycle
            Bin, and Hackerman keeps no copy.
          </p>
          <ul className="flex list-disc flex-col gap-2 ps-5">
            <li>
              <strong>Caches and downloaded packages</strong> come back by themselves, but the next
              install or build is slower, and needs the internet to download them again.
            </li>
            <li>
              <strong>Build folders</strong>, like <code>node_modules</code>, Rust's{" "}
              <code>target</code>, .NET's <code>bin</code> and <code>obj</code>, and Next.js's{" "}
              <code>.next</code>, have to be built or installed again before those projects run.
              For large projects that can take many minutes. Anything someone put in one of those
              folders by hand is deleted with it.
            </li>
            <li>
              <strong>Docker images you built yourself</strong> and never pushed anywhere have to
              be built again from their Dockerfile.
            </li>
            <li>
              <strong>Docker volumes and stopped containers</strong> can hold databases, uploaded
              files and other work that exists nowhere else. Hackerman never chooses these for
              you. They stay hidden until you ask to see them, and need typing{" "}
              <code>{CONFIRM_WORD}</code> to delete.
            </li>
            <li>
              <strong>A running program can break</strong> if its files are deleted under it.
              Hackerman leaves out any project a running program is using, but it can't know about
              everything, like an editor with the project open.
            </li>
            <li>
              <strong>Someone may be using {pcName}</strong> right now. They aren't asked first,
              though Hackerman tells them afterwards.
            </li>
            <li>
              Sizes are measured as Hackerman finds them. Folders that share files with others,
              as pnpm's do, free less than their size, and some of Docker's can only be guessed.
            </li>
          </ul>
          <p className="text-muted-foreground">
            You choose what's deleted, and you're responsible for that choice. Check the list on the
            next page. If you're not sure what something is, leave it.
          </p>
          <label htmlFor={id} className="flex items-center gap-2 rounded-md border px-3 py-2">
            <input
              id={id}
              type="checkbox"
              checked={understood}
              onChange={(event) => setUnderstood(event.target.checked)}
              className="size-4 accent-current"
            />
            I understand that what I delete is gone for good
          </label>
        </div>
      </ScrollArea>
      <div className="flex justify-end gap-2 border-t px-5 py-3">
        <Button variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button disabled={!understood} onClick={onContinue}>
          Continue
        </Button>
      </div>
    </>
  );
}

function Choosing({
  choices,
  dockerKnown,
  onCancel,
  onClean,
}: {
  choices: Choice[];
  dockerKnown: boolean;
  onCancel: () => void;
  onClean: (chosen: Choice[]) => void;
}) {
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [keepDays, setKeepDays] = useState("30");
  const [showRisky, setShowRisky] = useState(false);
  const [typed, setTyped] = useState("");
  const now = Date.now();
  const kept = (choice: Choice) => recentlyUsed(choice, Number(keepDays), now);
  const available = (choice: Choice) => choice.blocked === null && !kept(choice);
  const picked = choices.filter((choice) => chosen.has(choice.key) && available(choice));
  const { size, unknown, risky } = summary(picked);
  const confirmed = !risky || typed.trim() === CONFIRM_WORD;
  const toggle = (choice: Choice, on: boolean) =>
    setChosen((before) => {
      const next = new Set(before);
      if (on) next.add(choice.key);
      else next.delete(choice.key);
      return next;
    });
  const groups: Risk[] = showRisky ? ["safe", "slower", "canLoseData"] : ["safe", "slower"];

  return (
    <>
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex flex-col gap-4 px-5 py-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <label className="flex items-center gap-2 text-sm">
              Keep projects
              <Select value={keepDays} onValueChange={setKeepDays}>
                <SelectTrigger size="sm" className="w-56">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {KEEP.map((option) => (
                    <SelectItem key={option.value} value={option.value}>
                      {option.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </label>
            <Button size="xs" variant={showRisky ? "destructive" : "ghost"} aria-pressed={showRisky} onClick={() => setShowRisky(!showRisky)}>
              {showRisky ? "Hide risky options" : "Show risky options"}
            </Button>
          </div>
          {!dockerKnown && (
            <p className="text-xs text-muted-foreground">Docker isn't running there, so nothing of Docker's is listed.</p>
          )}
          {groups.map((risk) => {
            const inGroup = choices.filter((choice) => choice.risk === risk);
            if (inGroup.length === 0) return null;
            return (
              <section key={risk} className="flex flex-col gap-1.5">
                <h3 className={cn("text-sm font-medium", risk === "canLoseData" && "text-destructive")}>
                  {RISKS[risk].title}
                </h3>
                <p className="text-xs text-muted-foreground">{RISKS[risk].detail}</p>
                <ul className="flex flex-col divide-y rounded-md border">
                  {inGroup.map((choice) => {
                    const why = choice.blocked ?? (kept(choice) ? "Changed recently, so kept." : null);
                    return (
                      <li key={choice.key}>
                        <label className={cn("flex items-start gap-2.5 px-3 py-2", why && "opacity-60")}>
                          <input
                            type="checkbox"
                            disabled={why !== null}
                            checked={chosen.has(choice.key) && why === null}
                            onChange={(event) => toggle(choice, event.target.checked)}
                            className="mt-0.5 size-4 accent-current"
                          />
                          <span className="flex min-w-0 flex-1 flex-col">
                            <span className="truncate text-sm">{choice.label}</span>
                            <span className="truncate text-xs text-muted-foreground" title={choice.detail}>
                              {choice.detail}
                            </span>
                            <span className="text-xs text-muted-foreground">
                              {why ?? `Comes back with: ${choice.restore}`}
                              {choice.sharedFiles && " Shares files with pnpm's store, so frees less than this."}
                            </span>
                          </span>
                          <span className="shrink-0 text-sm tabular-nums">
                            {choice.size === null ? "?" : formatSize(choice.size)}
                          </span>
                        </label>
                      </li>
                    );
                  })}
                </ul>
              </section>
            );
          })}
        </div>
      </ScrollArea>
      <div className="flex flex-col gap-2 border-t px-5 py-3">
        {risky && (
          <label className="flex items-center gap-2 text-sm text-destructive">
            What you chose can lose data. Type {CONFIRM_WORD} to go ahead:
            <Input value={typed} onChange={(event) => setTyped(event.target.value)} className="h-7 w-32" aria-label={`Type ${CONFIRM_WORD}`} />
          </label>
        )}
        <div className="flex items-center justify-between gap-2">
          <span className="text-sm tabular-nums text-muted-foreground">
            {picked.length === 0
              ? "Nothing chosen"
              : `${picked.length} chosen, about ${formatSize(size)}${unknown > 0 ? ` and ${unknown} of unknown size` : ""}`}
          </span>
          <div className="flex gap-2">
            <Button variant="ghost" onClick={onCancel}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              disabled={picked.length === 0 || !confirmed}
              onClick={() => onClean(picked)}
            >
              Delete {picked.length === 1 ? "it" : `these ${picked.length}`} for good
            </Button>
          </div>
        </div>
      </div>
    </>
  );
}

function Results({
  results,
  chosen,
  problem,
  onClose,
}: {
  results: CleanResult[];
  chosen: Choice[];
  problem: string | null;
  onClose: () => void;
}) {
  const freed = results.reduce((total, result) => total + result.freed, 0);
  const choiceOf = (result: CleanResult) => chosen.find((choice) => choice.key === keyOf(result.item));
  return (
    <>
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex flex-col gap-3 px-5 py-4">
          {problem ? (
            <p role="alert" className="text-sm text-amber-700 dark:text-amber-400">
              {problem}
            </p>
          ) : (
            <p className="text-sm font-medium">Freed about {formatSize(freed)}.</p>
          )}
          <ul className="flex flex-col divide-y rounded-md border">
            {results.map((result) => (
              <li key={keyOf(result.item)} className="flex items-start justify-between gap-3 px-3 py-2">
                <span className="flex min-w-0 flex-col">
                  <span className="truncate text-sm">{choiceOf(result)?.label ?? keyOf(result.item)}</span>
                  {choiceOf(result) && (
                    <span className="text-xs text-muted-foreground">
                      To get it back: {choiceOf(result)?.restore}
                    </span>
                  )}
                  {result.problem && (
                    <span className="text-xs text-amber-700 dark:text-amber-400">{result.problem}</span>
                  )}
                </span>
                <span className="shrink-0 text-sm tabular-nums">{formatSize(result.freed)}</span>
              </li>
            ))}
          </ul>
          <p className="text-xs text-muted-foreground">
            Hackerman's log on that PC lists what was asked for, under Settings, About.
          </p>
        </div>
      </ScrollArea>
      <div className="flex justify-end border-t px-5 py-3">
        <Button onClick={onClose}>Close</Button>
      </div>
    </>
  );
}
