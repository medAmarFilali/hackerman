import { Check, Loader2, Pencil, X } from "lucide-react";
import { Fragment, useState } from "react";

import { DeviceIcon, NumberBadge, type Presence } from "@/components/Art";
import { TransfersPanel } from "@/components/TransfersPanel";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { activeDevice, cantReceiveFilesBecause, deviceNumbers, isLaptop } from "@/lib/devices";
import { useFileDrag } from "@/lib/file-drag";
import type { DeviceView, UiState } from "@/lib/types";
import { cn } from "@/lib/utils";
import { send } from "@/store";

export function DevicesPanel({ state }: { state: UiState }) {
  const paired = state.devices.filter((device) => device.status !== "available");
  const available = state.devices.filter((device) => device.status === "available");
  const numbers = deviceNumbers(state);
  const active = activeDevice(state);

  return (
    <aside className="flex min-h-0 flex-col border-l bg-card">
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex flex-col gap-6 p-4">
          {/* First, since progress is no use below the fold. */}
          {state.transfers.length > 0 && (
            <Section title="Files">
              <TransfersPanel state={state} />
            </Section>
          )}

          <Section title="Your PCs">
            <div className="flex flex-col">
              <ThisPc state={state} active={active === state.me.fingerprint} />
              {paired.map((device) => (
                <Fragment key={device.fingerprint}>
                  <Connector />
                  <PairedCard
                    device={device}
                    canReceive={!cantReceiveFilesBecause(state, device.fingerprint)}
                    number={numbers.get(device.fingerprint) ?? 0}
                    laptop={isLaptop(state, device.fingerprint)}
                    active={active === device.fingerprint}
                  />
                </Fragment>
              ))}
            </div>
          </Section>

          <Section title="On your network">
            {available.map((device) => (
              <AvailableRow key={device.fingerprint} device={device} />
            ))}
            {available.length === 0 && (
              <p className="flex items-start gap-2 text-xs text-muted-foreground">
                <Loader2 aria-hidden className="mt-px size-3.5 shrink-0 animate-spin" />
                Looking for other PCs running Hackerman…
              </p>
            )}
          </Section>
        </div>
      </ScrollArea>
      <AddByAddress />
    </aside>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="flex flex-col gap-2.5">
      <h2 className="eyebrow">{title}</h2>
      {children}
    </section>
  );
}

function Connector() {
  return <span aria-hidden className="mx-auto h-3 w-px bg-line" />;
}

function DeviceCard({
  device,
  canReceive,
  presence,
  number,
  laptop,
  title,
  action,
  children,
}: {
  device: string;
  canReceive: boolean;
  presence: Presence;
  number: number;
  laptop: boolean;
  title: React.ReactNode;
  action?: React.ReactNode;
  children: React.ReactNode;
}) {
  const active = presence === "active";
  const dropping = useFileDrag((drag) => drag.over === device);
  return (
    <div
      data-presence={presence}
      data-drop-device={device}
      data-drop={dropping ? (canReceive ? "ok" : "blocked") : undefined}
      className={cn(
        "frame relative",
        active && "frame-glow",
        presence === "away" && "border-dashed",
        dropping && canReceive && "ring-2 ring-signal",
        dropping && !canReceive && "ring-2 ring-destructive/60",
      )}
    >
      <NumberBadge number={number} active={active} className="absolute -top-1.5 -right-1.5" />
      <div className={cn("frame-inner flex items-start gap-2.5 p-2", presence === "away" && "opacity-70")}>
        <DeviceIcon laptop={laptop} active={active} />
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1">
            <div className="min-w-0 truncate text-sm font-medium">{title}</div>
            {action}
          </div>
          {children}
        </div>
      </div>
    </div>
  );
}

function ThisPc({ state, active }: { state: UiState; active: boolean }) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState("");
  const { me } = state;
  const [firstAddress, ...otherAddresses] = me.addresses;

  function save() {
    send({ type: "rename", name });
    setEditing(false);
  }

  if (editing) {
    return (
      <form
        className="flex items-center gap-1"
        onSubmit={(event) => {
          event.preventDefault();
          save();
        }}
      >
        <Input
          autoFocus
          aria-label="Name of this PC"
          value={name}
          maxLength={40}
          onChange={(event) => setName(event.target.value)}
          onKeyDown={(event) => event.key === "Escape" && setEditing(false)}
        />
        <Button type="submit" size="icon" variant="ghost" aria-label="Save name">
          <Check />
        </Button>
      </form>
    );
  }

  return (
    <DeviceCard
      device={me.fingerprint}
      canReceive={state.enabled}
      presence={state.enabled ? (active ? "active" : "present") : "away"}
      number={1}
      laptop={isLaptop(state, me.fingerprint)}
      title={me.name}
      action={
        <Tooltip>
          <TooltipTrigger asChild>
            <Button
              size="icon-xs"
              variant="ghost"
              aria-label="Rename this PC"
              className="-my-1 shrink-0 text-muted-foreground"
              onClick={() => {
                setName(me.name);
                setEditing(true);
              }}
            >
              <Pencil />
            </Button>
          </TooltipTrigger>
          <TooltipContent>Rename</TooltipContent>
        </Tooltip>
      }
    >
      {me.model && <div className="truncate text-xs text-muted-foreground">{me.model}</div>}
      <div className="truncate text-xs text-muted-foreground tabular-nums select-text">
        {state.enabled ? "This PC" : "Disabled"} · {firstAddress ?? "Not on a network"}
        {otherAddresses.length > 0 && (
          <Tooltip>
            <TooltipTrigger className="ml-1 underline decoration-dotted underline-offset-2">
              +{otherAddresses.length}
            </TooltipTrigger>
            <TooltipContent className="tabular-nums">{otherAddresses.join(", ")}</TooltipContent>
          </Tooltip>
        )}
      </div>
    </DeviceCard>
  );
}

function statusText(device: DeviceView): string {
  switch (device.status) {
    case "connected":
      return `Connected · ${device.addresses[0] ?? ""}`;
    case "off":
      return "Disabled on that PC";
    case "incompatible":
      return "Different version: update Hackerman on one of these PCs";
    default:
      return "Not connected";
  }
}

function PairedCard({
  device,
  canReceive,
  number,
  laptop,
  active,
}: {
  device: DeviceView;
  canReceive: boolean;
  number: number;
  laptop: boolean;
  active: boolean;
}) {
  const [confirming, setConfirming] = useState(false);
  return (
    <>
      <DeviceCard
        device={device.fingerprint}
        canReceive={canReceive}
        presence={device.status === "connected" ? (active ? "active" : "present") : "away"}
        number={number}
        laptop={laptop}
        title={device.name}
        action={
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                size="icon-xs"
                variant="ghost"
                aria-label={`Forget ${device.name}`}
                className="-my-1 ms-auto shrink-0 text-muted-foreground"
                onClick={() => setConfirming(true)}
              >
                <X />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Forget this PC</TooltipContent>
          </Tooltip>
        }
      >
        {device.model && <div className="truncate text-xs text-muted-foreground">{device.model}</div>}
        <div
          className={
            device.status === "incompatible"
              ? "text-xs font-medium text-amber-700 dark:text-amber-400"
              : "truncate text-xs text-muted-foreground tabular-nums"
          }
        >
          {statusText(device)}
        </div>
        {device.needsUpdateForFiles && (
          <div className="text-xs text-amber-700 dark:text-amber-400">
            Update it to share files. Everything else works.
          </div>
        )}
      </DeviceCard>

      <Dialog open={confirming} onOpenChange={setConfirming}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Forget {device.name}?</DialogTitle>
            <DialogDescription>
              It will stop sharing a keyboard and mouse with this PC. To connect them again you'd
              need to pair them with a new code.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirming(false)}>
              Keep it
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                send({ type: "forget", device: device.fingerprint });
                setConfirming(false);
              }}
            >
              Forget
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

function AvailableRow({ device }: { device: DeviceView }) {
  return (
    <div className="flex items-center gap-2 rounded-lg border border-dashed border-line/60 p-2">
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm font-medium">{device.name}</div>
        {device.model && <div className="truncate text-xs text-muted-foreground">{device.model}</div>}
        <div className="truncate text-xs text-muted-foreground tabular-nums">
          {device.addresses[0] ?? ""}
        </div>
      </div>
      <Button
        size="sm"
        variant="outline"
        onClick={() => send({ type: "pair", device: device.fingerprint })}
      >
        Pair
      </Button>
    </div>
  );
}

function AddByAddress() {
  const [address, setAddress] = useState("");
  return (
    <form
      className="flex flex-col gap-2 border-t p-4"
      onSubmit={(event) => {
        event.preventDefault();
        if (address.trim()) {
          send({ type: "pairAddress", address });
          setAddress("");
        }
      }}
    >
      <label htmlFor="pair-address" className="eyebrow">
        Pair by IP address
      </label>
      <div className="flex gap-2">
        <Input
          id="pair-address"
          value={address}
          placeholder="e.g. 192.168.1.20"
          autoComplete="off"
          spellCheck={false}
          className="tabular-nums"
          onChange={(event) => setAddress(event.target.value)}
        />
        <Button type="submit" variant="outline" disabled={!address.trim()}>
          Pair
        </Button>
      </div>
    </form>
  );
}
