import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { type ScreenPicture, followPicture, pictureSizeFor } from "@/lib/screen-pictures";
import type { PictureAnswer, ScreenAsk } from "@/lib/types";

vi.mock("@/store", () => ({ askForPicture: vi.fn() }));

const REQUEST = { device: "d", monitor: "m", maxWidth: 320, maxHeight: 200, cursor: false };
const PICTURE: PictureAnswer = {
  status: "picture",
  url: "data:image/jpeg;base64,AA==",
  width: 320,
  height: 180,
  fingerprint: 77,
};

/** Runs against a stand-in that gives these answers in turn, and then stays with the last. */
function following(answers: (PictureAnswer | Error)[], every = 1000) {
  const seen: ScreenPicture[] = [];
  const asks: ScreenAsk[] = [];
  let next = 0;
  const stop = followPicture(REQUEST, (picture) => seen.push(picture), {
    every,
    ask: async (ask) => {
      asks.push(ask);
      const answer = answers[Math.min(next++, answers.length - 1)];
      if (answer instanceof Error) throw answer;
      return answer;
    },
  });
  return { seen, asks, stop };
}

describe("following a screen's picture", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows a picture and says which one it has when asking again", async () => {
    const { seen, asks, stop } = following([PICTURE, { status: "unchanged" }]);
    await vi.advanceTimersByTimeAsync(0);
    expect(asks[0].known).toBe(0);
    expect(seen).toEqual([
      { kind: "picture", url: "data:image/jpeg;base64,AA==", width: 320, height: 180 },
    ]);

    await vi.advanceTimersByTimeAsync(1000);
    expect(asks[1].known).toBe(77);
    expect(seen).toHaveLength(1);
    stop();
  });

  it("asks once each time round, however long it goes on", async () => {
    const { asks, stop } = following([{ status: "unchanged" }]);
    await vi.advanceTimersByTimeAsync(5500);
    expect(asks).toHaveLength(6);
    stop();
  });

  it("stops asking when told to, and ignores an answer that comes after that", async () => {
    const seen: ScreenPicture[] = [];
    let answer: (value: PictureAnswer) => void = () => {};
    let asked = 0;
    const stop = followPicture(REQUEST, (picture) => seen.push(picture), {
      every: 100,
      ask: () => {
        asked++;
        return new Promise((resolve) => {
          answer = resolve;
        });
      },
    });
    await vi.advanceTimersByTimeAsync(0);
    expect(asked).toBe(1);
    stop();
    answer(PICTURE);
    await vi.advanceTimersByTimeAsync(1000);
    expect(seen).toEqual([]);
    expect(asked).toBe(1);
  });

  it("only asks again once the last one has been answered", async () => {
    let answer: (value: PictureAnswer) => void = () => {};
    let asked = 0;
    const stop = followPicture(REQUEST, () => {}, {
      every: 100,
      ask: () => {
        asked++;
        return new Promise((resolve) => {
          answer = resolve;
        });
      },
    });
    await vi.advanceTimersByTimeAsync(5000);
    expect(asked).toBe(1);
    answer({ status: "unchanged" });
    await vi.advanceTimersByTimeAsync(0);
    expect(asked).toBe(2);
    stop();
  });

  it("says why there's no picture, and takes it down", async () => {
    const { seen, stop } = following([PICTURE, { status: "unavailable" }, { status: "notAllowed" }]);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(1000);
    await vi.advanceTimersByTimeAsync(1000);
    expect(seen.slice(1)).toEqual([
      { kind: "none", why: "unavailable" },
      { kind: "none", why: "notAllowed" },
    ]);
    stop();
  });

  it("starts from nothing after a gap, as what it had is no longer on show", async () => {
    const { asks, stop } = following([PICTURE, { status: "unavailable" }, PICTURE]);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(1000);
    await vi.advanceTimersByTimeAsync(1000);
    expect(asks.map((ask) => ask.known)).toEqual([0, 77, 0]);
    stop();
  });

  it("asks less often when told to wait, or told it isn't allowed", async () => {
    const busy = following([{ status: "busy" }], 100);
    await vi.advanceTimersByTimeAsync(950);
    expect(busy.asks).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(100);
    expect(busy.asks).toHaveLength(2);
    busy.stop();

    const refused = following([{ status: "notAllowed" }], 100);
    await vi.advanceTimersByTimeAsync(4900);
    expect(refused.asks).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(200);
    expect(refused.asks).toHaveLength(2);
    refused.stop();
  });

  it("keeps what it has and tries again when there's no answer at all", async () => {
    const { seen, asks, stop } = following([PICTURE, new Error("out of sight"), PICTURE], 100);
    await vi.advanceTimersByTimeAsync(0);
    await vi.advanceTimersByTimeAsync(100);
    expect(seen).toHaveLength(1);
    expect(asks).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(900);
    expect(asks).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(150);
    expect(asks).toHaveLength(3);
    // What it had may well still be on show, so it needn't be sent again if so.
    expect(asks[2].known).toBe(77);
    stop();
  });
});

describe("how big a picture to ask for", () => {
  it("goes by the pixels on show, in steps", () => {
    expect(pictureSizeFor(200, 120, 1)).toEqual({ maxWidth: 240, maxHeight: 160 });
    expect(pictureSizeFor(201, 121, 1)).toEqual({ maxWidth: 240, maxHeight: 160 });
    expect(pictureSizeFor(200, 120, 2)).toEqual({ maxWidth: 400, maxHeight: 240 });
  });

  it("stays within sensible limits", () => {
    expect(pictureSizeFor(10, 10, 1)).toEqual({ maxWidth: 160, maxHeight: 160 });
    expect(pictureSizeFor(5000, 5000, 2)).toEqual({ maxWidth: 640, maxHeight: 640 });
  });
});
