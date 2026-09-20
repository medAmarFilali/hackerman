import { describe, expect, it } from "vitest";

import {
  type DeviceRect,
  findCrossings,
  fitView,
  resizedWidth,
  resizeLimits,
  resolveOverlaps,
  screensChanged,
  snapDrag,
  snapResize,
  visibleCrossings,
} from "./canvas";

function screen(device: string, x: number, y: number, width = 1920, height = 1080, online = true): DeviceRect {
  return { device, x, y, width, height, online };
}

describe("findCrossings", () => {
  it("finds a shared vertical edge", () => {
    const crossings = findCrossings([screen("a", 0, 0), screen("b", 1920, 200)]);
    expect(crossings).toEqual([
      {
        orientation: "vertical",
        x: 1920,
        y: 200,
        length: 880,
        gap: 0,
        active: true,
        sameDevice: false,
        devices: ["a", "b"],
      },
    ]);
  });

  it("finds a shared horizontal edge", () => {
    const crossings = findCrossings([screen("a", 0, 0), screen("b", 500, 1080)]);
    expect(crossings).toEqual([
      {
        orientation: "horizontal",
        x: 500,
        y: 1080,
        length: 1420,
        gap: 0,
        active: true,
        sameDevice: false,
        devices: ["a", "b"],
      },
    ]);
  });

  it("works in either order", () => {
    const crossings = findCrossings([screen("b", 1920, 0), screen("a", 0, 0)]);
    expect(crossings).toHaveLength(1);
    expect(crossings[0].x).toBe(1920);
  });

  it("leaves out neighbouring screens of one device", () => {
    expect(findCrossings([screen("a", 0, 0), screen("a", 1920, 0)])).toEqual([]);
  });

  it("shows a gap between screens of one device as a crossing", () => {
    const [crossing] = findCrossings([screen("a", 0, 0), screen("a", 5000, 0)]);
    expect(crossing.sameDevice).toBe(true);
    expect(crossing.gap).toBe(5000 - 1920);
  });

  it("ignores screens that only meet at a corner", () => {
    expect(findCrossings([screen("a", 0, 0), screen("b", 1920, 1080)])).toEqual([]);
  });

  it("ignores screens that aren't in line at all", () => {
    expect(findCrossings([screen("a", 0, 0), screen("b", 5000, 5000)])).toEqual([]);
  });

  it("connects screens across a gap of any size", () => {
    const [crossing] = findCrossings([screen("a", 0, 0), screen("b", 9000, 100)]);
    expect(crossing).toMatchObject({
      orientation: "vertical",
      x: 1920,
      y: 100,
      length: 980,
      gap: 9000 - 1920,
      active: true,
    });
  });

  it("connects only to the nearest screen in line", () => {
    const crossings = findCrossings([screen("a", 0, 0), screen("b", 3000, 0), screen("c", 6000, 0)]);
    expect(crossings.map((c) => c.devices)).toEqual([
      ["a", "b"],
      ["b", "c"],
    ]);
  });

  it("reaches past a short screen where nothing is in the way", () => {
    const crossings = findCrossings([
      screen("a", 0, 0),
      screen("b", 3000, 0, 1920, 500),
      screen("c", 6000, 0),
    ]);
    const aToC = crossings.find((c) => c.devices[0] === "a" && c.devices[1] === "c");
    expect(aToC).toMatchObject({ y: 500, length: 580 });
    const aToB = crossings.find((c) => c.devices[0] === "a" && c.devices[1] === "b");
    expect(aToB).toMatchObject({ y: 0, length: 500 });
  });

  it("looks straight through offline devices", () => {
    const crossings = findCrossings([
      screen("a", 0, 0),
      screen("b", 3000, 0, 1920, 1080, false),
      screen("c", 6000, 0),
    ]);
    const active = crossings.filter((c) => c.active);
    expect(active.map((c) => c.devices)).toEqual([["a", "c"]]);
  });

  it("counts screens overlapping within the engine's tolerance as ahead", () => {
    expect(findCrossings([screen("a", 0, 0), screen("b", 1919.5, 0)])).toHaveLength(1);
  });

  it("marks crossings with an offline device as inactive", () => {
    const [crossing] = findCrossings([screen("a", 0, 0), screen("b", 1920, 0, 1920, 1080, false)]);
    expect(crossing.active).toBe(false);
  });
});

describe("visibleCrossings", () => {
  it("leaves out inactive crossings under an active one", () => {
    const crossings = findCrossings([
      screen("a", 0, 0),
      screen("b", 3000, 0, 1920, 1080, false),
      screen("c", 6000, 0),
    ]);
    expect(visibleCrossings(crossings).map((c) => c.devices)).toEqual([["a", "c"]]);
  });

  it("keeps inactive crossings that nothing covers", () => {
    const crossings = findCrossings([screen("a", 0, 0), screen("b", 3000, 0, 1920, 1080, false)]);
    expect(visibleCrossings(crossings)).toEqual(crossings);
  });

  it("isn't fooled by an active crossing elsewhere, or going the other way", () => {
    const crossings = findCrossings([
      screen("a", 0, 0),
      screen("b", 3000, 0),
      screen("c", 0, 2000, 1920, 1080, false),
    ]);
    const inactive = visibleCrossings(crossings).filter((c) => !c.active);
    expect(inactive.map((c) => c.devices)).toEqual([["a", "c"]]);
  });
});

describe("resizedWidth", () => {
  const box = { x: 0, y: 0, width: 1600, height: 1000 };

  it("stays the same when the corner hasn't moved", () => {
    expect(resizedWidth(box, 0, 0)).toBe(1600);
  });

  it("follows a corner dragged along the screen's diagonal exactly", () => {
    expect(resizedWidth(box, 160, 100)).toBeCloseTo(1760);
    expect(resizedWidth(box, -160, -100)).toBeCloseTo(1440);
  });

  it("ignores movement across the diagonal", () => {
    expect(resizedWidth(box, 100, -160)).toBeCloseTo(1600);
  });
});

describe("snapResize", () => {
  const box = { x: 0, y: 0, width: 1600, height: 1000 };

  it("sticks the right edge to a nearby edge", () => {
    expect(snapResize(box, [screen("b", 2000, 0)], 1990, 20)).toBe(2000);
  });

  it("sticks the bottom edge to a nearby edge, keeping the shape", () => {
    // A bottom edge at 1500 means a height of 1500, so a width of 2400.
    expect(snapResize(box, [screen("b", 9000, 1500)], 2390, 20)).toBe(2400);
  });

  it("leaves the size alone when nothing is close", () => {
    expect(snapResize(box, [screen("b", 5000, 5000)], 1700, 20)).toBe(1700);
  });
});

describe("resizeLimits", () => {
  it("allows a quarter to four times the usual size", () => {
    expect(resizeLimits(1600)).toEqual({ min: 400, max: 6400 });
  });
});

describe("snapDrag", () => {
  const still = [screen("a", 0, 0)];
  const moving = [screen("b", 3000, 0)];

  it("sticks to a nearby edge", () => {
    // Dragged to 10 units short of touching a's right edge.
    expect(snapDrag(moving, still, 1920 + 10 - 3000, 0, 20).dx).toBe(1920 - 3000);
  });

  it("leaves the drag alone when nothing is close", () => {
    expect(snapDrag(moving, still, -500, 0, 20).dx).toBe(-500);
  });

  it("lines up with the other monitor's top", () => {
    const result = snapDrag(moving, still, 1920 - 3000, 12, 20);
    expect(result.dy).toBe(0);
  });

  it("ignores edges far away in the other direction", () => {
    // Directly below by a long way, so a's right edge shouldn't attract it.
    const result = snapDrag([screen("b", 3000, 5000)], still, 1930 - 3000, 0, 20);
    expect(result.dx).toBe(1930 - 3000);
  });

  it("uses the moving group's outer edges", () => {
    const group = [screen("b", 3000, 0), screen("b", 4920, 0)];
    // The group's right edge lands 10 short of a monitor on the far side.
    const far = [screen("c", 7000, 0)];
    const result = snapDrag(group, far, 7000 - 6840 - 10, 0, 20);
    expect(result.dx).toBe(7000 - 6840);
  });
});

describe("resolveOverlaps", () => {
  it("leaves separate monitors alone", () => {
    expect(resolveOverlaps([screen("b", 1920, 0)], [screen("a", 0, 0)])).toEqual({ dx: 0, dy: 0 });
  });

  it("pushes out by the shortest distance", () => {
    // Overlapping a's right side by 100.
    expect(resolveOverlaps([screen("b", 1820, 0)], [screen("a", 0, 0)])).toEqual({ dx: 100, dy: 0 });
    // Overlapping a's bottom by 80.
    expect(resolveOverlaps([screen("b", 0, 1000)], [screen("a", 0, 0)])).toEqual({ dx: 0, dy: 80 });
  });

  it("ends with no overlap when pushed from one monitor into another", () => {
    const others = [screen("a", 0, 0), screen("c", 1920, 0)];
    const moving = [screen("b", 1000, 0)];
    const { dx, dy } = resolveOverlaps(moving, others);
    const placed = { ...moving[0], x: moving[0].x + dx, y: moving[0].y + dy };
    for (const other of others) {
      const separate =
        placed.x >= other.x + other.width ||
        placed.x + placed.width <= other.x ||
        placed.y >= other.y + other.height ||
        placed.y + placed.height <= other.y;
      expect(separate).toBe(true);
    }
  });

  it("moves a whole group together", () => {
    const group = [screen("b", 1820, 0), screen("b", 3740, 0)];
    expect(resolveOverlaps(group, [screen("a", 0, 0)])).toEqual({ dx: 100, dy: 0 });
  });
});

describe("screensChanged", () => {
  const on = (device: string, monitor: string) => ({ device, monitor });

  it("is quiet when the same screens are there", () => {
    const screens = [on("a", "one"), on("a", "two"), on("b", "three")];
    expect(screensChanged(screens, [...screens].reverse())).toBe(false);
  });

  it("notices a screen plugged in or unplugged", () => {
    const one = [on("a", "one")];
    const two = [on("a", "one"), on("a", "two")];
    expect(screensChanged(one, two)).toBe(true);
    expect(screensChanged(two, one)).toBe(true);
  });

  it("notices a screen replaced by another", () => {
    expect(screensChanged([on("a", "one")], [on("a", "two")])).toBe(true);
  });

  it("isn't about PCs being paired or forgotten", () => {
    const here = [on("a", "one")];
    const withAnother = [on("a", "one"), on("b", "two")];
    expect(screensChanged(here, withAnother)).toBe(false);
    expect(screensChanged(withAnother, here)).toBe(false);
  });

  it("has nothing to say the first time there are any", () => {
    expect(screensChanged([], [on("a", "one")])).toBe(false);
  });
});

describe("fitView", () => {
  it("centres everything in the area", () => {
    const view = fitView([screen("a", 0, 0, 1000, 500)], 800, 600, 100);
    expect(view.scale).toBe(0.2);
    expect(view.offsetX).toBe(300);
    expect(view.offsetY).toBe(250);
  });

  it("shrinks to fit a wide layout, including negative positions", () => {
    const view = fitView([screen("a", -2000, 0, 2000, 1000), screen("b", 0, 0, 2000, 1000)], 500, 500, 50);
    expect(view.scale).toBe(0.1);
    // The whole layout, -2000 to 2000, spans the middle 400 of the 500-wide area.
    expect(-2000 * view.scale + view.offsetX).toBe(50);
    expect(2000 * view.scale + view.offsetX).toBe(450);
  });

  it("copes with nothing to show", () => {
    expect(fitView([], 500, 500, 50).scale).toBeGreaterThan(0);
  });
});
