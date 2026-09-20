// Geometry of the monitor canvas, in canvas units (logical pixels).

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** A rectangle that belongs to a device. */
export interface DeviceRect extends Rect {
  device: string;
  online: boolean;
}

/**
 * A band across which the cursor passes from one screen to another. Screens don't
 * have to touch: leaving an edge goes to the nearest screen straight out from it.
 */
export interface Crossing {
  orientation: "vertical" | "horizontal";
  /** Where the band starts: its top end if vertical, its left end if horizontal. */
  x: number;
  y: number;
  length: number;
  /** How far apart the two screens are: the band's thickness. */
  gap: number;
  active: boolean;
  /** Between two screens of one device. */
  sameDevice: boolean;
  devices: [string, string];
}

/** Must match TOUCH_TOLERANCE in crates/hk-core/src/layout.rs. */
const TOUCH_TOLERANCE = 1;

const right = (r: Rect) => r.x + r.width;
const bottom = (r: Rect) => r.y + r.height;

/** What's left of the spans after taking the range from `start` to `end` out of them. */
function subtract(spans: [number, number][], start: number, end: number): [number, number][] {
  return spans.flatMap(([from, to]): [number, number][] => {
    if (end <= from || start >= to) return [[from, to]];
    const pieces: [number, number][] = [];
    if (start > from) pieces.push([from, start]);
    if (end < to) pieces.push([end, to]);
    return pieces;
  });
}

/**
 * Every place where the cursor can pass between two screens, matching what
 * `Topology::crossing` does in crates/hk-core/src/layout.rs. Neighbouring screens
 * of one device are left out, since the cursor moving between those is nothing new.
 */
export function findCrossings(screens: DeviceRect[]): Crossing[] {
  const crossings: Crossing[] = [];

  function connect(
    first: DeviceRect,
    second: DeviceRect,
    orientation: Crossing["orientation"],
    gap: number,
    shared: [number, number],
    gapTo: (other: DeviceRect) => number,
    spanOf: (other: DeviceRect) => [number, number],
  ) {
    const active = first.online && second.online;
    const sameDevice = first.device === second.device;
    if (sameDevice && gap <= TOUCH_TOLERANCE) return;

    // Anything nearer that's in line blocks the view of the second screen. Devices
    // that are offline don't, as the cursor passes straight over them.
    let spans: [number, number][] = [shared];
    for (const other of screens) {
      if (other === first || other === second) continue;
      if (active && !other.online) continue;
      const otherGap = gapTo(other);
      if (otherGap >= -TOUCH_TOLERANCE && otherGap < gap) {
        const [start, end] = spanOf(other);
        spans = subtract(spans, start, end);
      }
    }

    for (const [start, end] of spans) {
      const devices: [string, string] = [first.device, second.device];
      crossings.push(
        orientation === "vertical"
          ? { orientation, x: right(first), y: start, length: end - start, gap, active, sameDevice, devices }
          : { orientation, x: start, y: bottom(first), length: end - start, gap, active, sameDevice, devices },
      );
    }
  }

  for (const a of screens) {
    for (const b of screens) {
      if (a === b) continue;

      // b to the right of a.
      const across = b.x - right(a);
      const top = Math.max(a.y, b.y);
      const down = Math.min(bottom(a), bottom(b));
      if (across >= -TOUCH_TOLERANCE && down > top) {
        connect(
          a,
          b,
          "vertical",
          across,
          [top, down],
          (other) => other.x - right(a),
          (other) => [other.y, bottom(other)],
        );
      }

      // b below a.
      const below = b.y - bottom(a);
      const left = Math.max(a.x, b.x);
      const end = Math.min(right(a), right(b));
      if (below >= -TOUCH_TOLERANCE && end > left) {
        connect(
          a,
          b,
          "horizontal",
          below,
          [left, end],
          (other) => other.y - bottom(a),
          (other) => [other.x, right(other)],
        );
      }
    }
  }
  return crossings;
}

/**
 * Leaves out inactive crossings lying under an active one. Past an offline device,
 * the active one is what actually happens there, and both would only be clutter.
 */
export function visibleCrossings(crossings: Crossing[]): Crossing[] {
  const extent = (crossing: Crossing) =>
    crossing.orientation === "vertical"
      ? { across: crossing.x, along: crossing.y }
      : { across: crossing.y, along: crossing.x };
  const active = crossings.filter((crossing) => crossing.active);
  return crossings.filter((crossing) => {
    if (crossing.active) return true;
    const it = extent(crossing);
    return !active.some((other) => {
      const them = extent(other);
      return (
        other.orientation === crossing.orientation &&
        them.across < it.across + crossing.gap &&
        it.across < them.across + other.gap &&
        them.along < it.along + crossing.length &&
        it.along < them.along + other.length
      );
    });
  });
}

/**
 * Whether a PC has different screens than before: one plugged in or unplugged. A PC being
 * paired or forgotten isn't that, as the PC itself came or went.
 */
export function screensChanged(
  before: { device: string; monitor: string }[],
  after: { device: string; monitor: string }[],
): boolean {
  const monitorsOf = (screens: { device: string; monitor: string }[], device: string) =>
    screens
      .filter((screen) => screen.device === device)
      .map((screen) => screen.monitor)
      .sort()
      .join("\n");
  const pcsBefore = new Set(before.map((screen) => screen.device));
  return [...new Set(after.map((screen) => screen.device))].some(
    (device) => pcsBefore.has(device) && monitorsOf(before, device) !== monitorsOf(after, device),
  );
}

export interface View {
  scale: number;
  offsetX: number;
  offsetY: number;
}

/** The scale and offset that fit all `rects` centred in the given area. */
export function fitView(rects: Rect[], areaWidth: number, areaHeight: number, padding: number): View {
  if (rects.length === 0 || areaWidth <= 0 || areaHeight <= 0) {
    return { scale: 0.1, offsetX: areaWidth / 2, offsetY: areaHeight / 2 };
  }
  const minX = Math.min(...rects.map((r) => r.x));
  const minY = Math.min(...rects.map((r) => r.y));
  const maxX = Math.max(...rects.map(right));
  const maxY = Math.max(...rects.map(bottom));
  const width = maxX - minX;
  const height = maxY - minY;
  // A single small monitor shouldn't fill the window.
  const maximumScale = 0.2;
  const scale = Math.min(
    maximumScale,
    Math.max(areaWidth - padding * 2, 1) / width,
    Math.max(areaHeight - padding * 2, 1) / height,
  );
  return {
    scale,
    offsetX: (areaWidth - width * scale) / 2 - minX * scale,
    offsetY: (areaHeight - height * scale) / 2 - minY * scale,
  };
}

function overlapping(a: Rect, b: Rect): boolean {
  return a.x < right(b) && right(a) > b.x && a.y < bottom(b) && bottom(a) > b.y;
}

function moved(r: Rect, dx: number, dy: number): Rect {
  return { ...r, x: r.x + dx, y: r.y + dy };
}

/** The smallest of `candidates` within `threshold` of zero, or zero. */
function closest(candidates: number[], threshold: number): number {
  let best = 0;
  let bestDistance = threshold;
  for (const candidate of candidates) {
    const distance = Math.abs(candidate);
    if (distance <= bestDistance) {
      best = candidate;
      bestDistance = distance;
    }
  }
  return best;
}

/**
 * Adjusts a drag so the moving rectangles' edges stick to nearby edges of the
 * others, making it easy to line screens up exactly.
 */
export function snapDrag(
  moving: Rect[],
  others: Rect[],
  dx: number,
  dy: number,
  threshold: number,
): { dx: number; dy: number } {
  const xCandidates: number[] = [];
  const yCandidates: number[] = [];
  for (const start of moving) {
    const m = moved(start, dx, dy);
    for (const o of others) {
      // Only edges that are level with each other, or nearly, are worth sticking to.
      if (m.y < bottom(o) + threshold && bottom(m) > o.y - threshold) {
        xCandidates.push(right(o) - m.x, o.x - right(m), o.x - m.x, right(o) - right(m));
      }
      if (m.x < right(o) + threshold && right(m) > o.x - threshold) {
        yCandidates.push(bottom(o) - m.y, o.y - bottom(m), o.y - m.y, bottom(o) - bottom(m));
      }
    }
  }
  return {
    dx: dx + closest(xCandidates, threshold),
    dy: dy + closest(yCandidates, threshold),
  };
}

/**
 * The width that puts a rectangle's bottom-right corner as close as its shape allows
 * to where that corner was dragged, `dx` and `dy` away.
 */
export function resizedWidth(rect: Rect, dx: number, dy: number): number {
  const { width, height } = rect;
  return width * (1 + (dx * width + dy * height) / (width * width + height * height));
}

/**
 * Adjusts the width of a rectangle being resized from its bottom-right corner, so
 * that its right or bottom edge sticks to nearby edges of the others. Its shape
 * stays the same, so the height follows the width.
 */
export function snapResize(rect: Rect, others: Rect[], width: number, threshold: number): number {
  const heightPerWidth = rect.height / rect.width;
  const candidates: number[] = [];
  for (const o of others) {
    for (const edge of [o.x, right(o)]) {
      candidates.push(edge - rect.x - width);
    }
    for (const edge of [o.y, bottom(o)]) {
      candidates.push((edge - rect.y) / heightPerWidth - width);
    }
  }
  return width + closest(candidates, threshold);
}

/** How large a screen can be drawn, as a width, compared with its usual size. */
export function resizeLimits(usualWidth: number): { min: number; max: number } {
  return { min: usualWidth / 4, max: usualWidth * 4 };
}

/**
 * Pushes the moving rectangles the shortest distance out of any they overlap,
 * so two monitors never sit on top of each other. Returns the total offset.
 */
export function resolveOverlaps(moving: Rect[], others: Rect[]): { dx: number; dy: number } {
  let dx = 0;
  let dy = 0;
  // Moving out of one can move into another, but it soon settles on an edge.
  for (let attempt = 0; attempt < 32; attempt++) {
    let hit: { m: Rect; o: Rect } | null = null;
    for (const start of moving) {
      const m = moved(start, dx, dy);
      const o = others.find((other) => overlapping(m, other));
      if (o) {
        hit = { m, o };
        break;
      }
    }
    if (!hit) break;
    const { m, o } = hit;
    const escapes = [
      { dx: right(o) - m.x, dy: 0 },
      { dx: o.x - right(m), dy: 0 },
      { dx: 0, dy: bottom(o) - m.y },
      { dx: 0, dy: o.y - bottom(m) },
    ];
    const shortest = escapes.reduce((best, escape) =>
      Math.abs(escape.dx) + Math.abs(escape.dy) < Math.abs(best.dx) + Math.abs(best.dy) ? escape : best,
    );
    dx += shortest.dx;
    dy += shortest.dy;
  }
  return { dx, dy };
}
