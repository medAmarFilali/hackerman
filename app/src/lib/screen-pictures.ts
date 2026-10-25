import { useEffect, useState } from "react";

import type { PictureAnswer, ScreenAsk } from "@/lib/types";
import { askForPicture } from "@/store";

/** What to show for a screen. */
export type ScreenPicture =
  | { kind: "waiting" }
  | { kind: "picture"; url: string; width: number; height: number }
  /** There's no picture, and this says why. */
  | { kind: "none"; why: "notAllowed" | "needsUpdate" | "unavailable" };

/** What's asked for, leaving out what the next picture depends on. */
export type PictureRequest = Omit<ScreenAsk, "known">;

export interface FollowOptions {
  /** How long to leave between asking, in milliseconds, counted from when the last was asked. */
  every: number;
  /** How long to wait before the first one, so that several screens don't all ask at once. */
  startAfter?: number;
  ask?: (ask: ScreenAsk) => Promise<PictureAnswer>;
}

/** How long to wait after being told it's busy, or getting no answer. */
const BACK_OFF_MS = 1000;
/** Nobody's going to allow it this second, so asking needn't be as often as for a live picture. */
const NOT_ALLOWED_EVERY_MS = 5000;

/**
 * Keeps asking for a picture until told to stop, one at a time. Each answer is what decides when
 * to ask again, so a slow connection simply means fewer pictures.
 */
export function followPicture(
  request: PictureRequest,
  onChange: (picture: ScreenPicture) => void,
  { every, startAfter = 0, ask = askForPicture }: FollowOptions,
): () => void {
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let known = 0;

  const next = (after: number) => {
    timer = setTimeout(() => void askOnce(), after);
  };

  const askOnce = async () => {
    const started = Date.now();
    let wait = every;
    try {
      const answer = await ask({ ...request, known });
      if (stopped) return;
      switch (answer.status) {
        case "picture":
          known = answer.fingerprint;
          onChange({
            kind: "picture",
            url: answer.url,
            width: answer.width,
            height: answer.height,
          });
          break;
        case "unchanged":
          break;
        case "busy":
          wait = Math.max(every, BACK_OFF_MS);
          break;
        case "notAllowed":
          known = 0;
          wait = Math.max(every, NOT_ALLOWED_EVERY_MS);
          onChange({ kind: "none", why: "notAllowed" });
          break;
        case "needsUpdate":
        case "unavailable":
          known = 0;
          wait = Math.max(every, BACK_OFF_MS);
          onChange({ kind: "none", why: answer.status });
          break;
      }
    } catch {
      // The window's out of sight, or the agent isn't answering. What's showing stays, as nothing
      // says it's wrong, and it'll be replaced as soon as there's an answer.
      if (stopped) return;
      wait = Math.max(every, BACK_OFF_MS);
    }
    next(Math.max(0, wait - (Date.now() - started)));
  };

  next(startAfter);
  return () => {
    stopped = true;
    clearTimeout(timer);
  };
}

/** What's on a screen right now. With nothing to ask for, this leaves it at waiting. */
export function useScreenPicture(
  request: PictureRequest | null,
  every: number,
  startAfter = 0,
): ScreenPicture {
  const [picture, setPicture] = useState<ScreenPicture>({ kind: "waiting" });
  const key = request && JSON.stringify(request);
  useEffect(() => {
    if (!request) {
      setPicture({ kind: "waiting" });
      return;
    }
    return followPicture(request, setPicture, { every, startAfter });
  }, [key, every, startAfter]);
  return picture;
}

/** A picture this size is plenty for a screen drawn this size, and it changes in big enough steps
 * not to start over at every small change in the size of the window. */
export function pictureSizeFor(cssWidth: number, cssHeight: number, pixelRatio: number) {
  const STEP = 80;
  const SMALLEST = 160;
  const LARGEST_SMALL_PICTURE = 640;
  const fit = (side: number) =>
    Math.min(
      LARGEST_SMALL_PICTURE,
      Math.max(SMALLEST, Math.ceil((side * pixelRatio) / STEP) * STEP),
    );
  return { maxWidth: fit(cssWidth), maxHeight: fit(cssHeight) };
}
