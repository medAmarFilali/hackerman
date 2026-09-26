// The line drawings: screens seen from the front, in the one line weight.

import { Laptop, Link2, Monitor } from "lucide-react";

import { cn } from "@/lib/utils";

export type Presence = "active" | "present" | "away";

function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

/**
 * What a screen stands on, drawn under it: a laptop's base, or a monitor's neck and
 * foot. `width` and `height` are the size of the screen.
 */
export function ScreenStand({
  laptop,
  width,
  height,
}: {
  laptop: boolean;
  width: number;
  height: number;
}) {
  const middle = width / 2;
  if (laptop) {
    const overhang = clamp(width * 0.05, 3, 14);
    const notch = clamp(width * 0.09, 6, 22);
    return (
      <svg aria-hidden width={width} height={9} className="block overflow-visible" fill="none" stroke="currentColor">
        <rect
          x={0.5 - overhang}
          y={1.5}
          width={width - 1 + overhang * 2}
          height={6}
          rx={3}
          fill="var(--card)"
        />
        <path
          d={`M ${middle - notch} 1.5 v 0.5 a 2 2 0 0 0 2 2 H ${middle + notch - 2} a 2 2 0 0 0 2 -2 v -0.5`}
        />
      </svg>
    );
  }
  const neck = clamp(width * 0.1, 8, 30) / 2;
  const neckHeight = clamp(height * 0.1, 5, 16);
  const foot = clamp(width * 0.36, 24, 110) / 2;
  return (
    <svg
      aria-hidden
      width={width}
      height={neckHeight + 5}
      className="block overflow-visible"
      fill="none"
      stroke="currentColor"
    >
      <path d={`M ${middle - neck} 0 V ${neckHeight} M ${middle + neck} 0 V ${neckHeight}`} />
      <rect
        x={middle - foot + 0.5}
        y={neckHeight + 0.5}
        width={foot * 2 - 1}
        height={4}
        rx={2}
        fill="var(--card)"
      />
    </svg>
  );
}

/** A PC's number, sitting on the corner of whatever stands for it. */
export function NumberBadge({
  number,
  active,
  className,
}: {
  number: number;
  active: boolean;
  className?: string;
}) {
  return (
    <span
      aria-hidden
      className={cn(
        "flex size-[1.125rem] items-center justify-center rounded-full border font-mono text-[0.625rem] font-medium tabular-nums",
        active
          ? "border-transparent bg-signal text-signal-foreground"
          : "border-(--frame) bg-card text-muted-foreground",
        className,
      )}
    >
      {number}
    </span>
  );
}

/** A laptop or monitor in a small square. */
export function DeviceIcon({
  laptop,
  active,
  className,
}: {
  laptop: boolean;
  active: boolean;
  className?: string;
}) {
  const Icon = laptop ? Laptop : Monitor;
  return (
    <span
      aria-hidden
      className={cn(
        "flex size-8 shrink-0 items-center justify-center rounded-md",
        active ? "bg-signal-soft text-signal" : "bg-muted text-muted-foreground",
        className,
      )}
    >
      <Icon className="size-4" strokeWidth={1.5} />
    </span>
  );
}

function SmallScreen({ x }: { x: number }) {
  return (
    <g>
      <rect x={x} y={6} width={68} height={42} rx={5} fill="var(--card)" />
      <rect x={x + 3} y={9} width={62} height={36} rx={3} stroke="var(--border)" />
      <path d={`M ${x + 29} 48 V 54 M ${x + 39} 48 V 54`} />
      <rect x={x + 21} y={54} width={26} height={4} rx={2} fill="var(--card)" />
    </g>
  );
}

/** Two PCs, joined up through a link in the middle once `live`. */
export function PairingArt({ live }: { live: boolean }) {
  const link = live ? "var(--signal)" : "var(--line)";
  return (
    <svg
      aria-hidden
      viewBox="0 0 260 64"
      className="mx-auto h-16 w-[16.25rem] text-line"
      fill="none"
      stroke="currentColor"
      strokeLinecap="round"
    >
      <SmallScreen x={12} />
      <SmallScreen x={180} />
      <g stroke={link} strokeDasharray={live ? undefined : "3 3"}>
        <path d="M 80 36 H 90 a 6 6 0 0 0 6 -6 v -2 a 6 6 0 0 1 6 -6 H 116" />
        <path d="M 180 36 H 170 a 6 6 0 0 1 -6 -6 v -2 a 6 6 0 0 0 -6 -6 H 144" />
      </g>
      <rect x={116} y={8} width={28} height={28} rx={7} fill="var(--card)" stroke={link} />
      <Link2 x={123} y={15} width={14} height={14} stroke={link} strokeWidth={1.5} />
    </svg>
  );
}
