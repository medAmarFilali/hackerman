import { Eye, Loader2, MonitorOff } from "lucide-react";
import { useEffect, useState } from "react";

import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { useScreenPicture } from "@/lib/screen-pictures";
import type { ScreenView } from "@/lib/types";

/** About ten pictures a second, if the PC and the network can keep up. */
const LIVE_EVERY_MS = 100;
const LARGEST_SIDE = 2560;

/** One screen in a large window, live, for looking and not touching. */
export function ScreenViewer({
  screen,
  deviceName,
  isThisPc,
  onClose,
}: {
  screen: ScreenView;
  deviceName: string;
  isThisPc: boolean;
  onClose: () => void;
}) {
  // Kept as state because a dialog's content only appears after the dialog itself has.
  const [area, setArea] = useState<HTMLDivElement | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  useEffect(() => {
    if (!area) return;
    const observer = new ResizeObserver(([entry]) => {
      setSize({ width: entry.contentRect.width, height: entry.contentRect.height });
    });
    observer.observe(area);
    return () => observer.disconnect();
  }, [area]);

  const ratio = window.devicePixelRatio || 1;
  const fit = (side: number) => Math.min(LARGEST_SIDE, Math.ceil((side * ratio) / 160) * 160);
  const picture = useScreenPicture(
    size.width > 0
      ? {
          device: screen.device,
          monitor: screen.monitor,
          maxWidth: fit(size.width),
          maxHeight: fit(size.height),
          cursor: true,
        }
      : null,
    LIVE_EVERY_MS,
  );

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex h-[calc(100vh-3rem)] max-w-[calc(100vw-3rem)] flex-col gap-3 p-4 sm:max-w-[calc(100vw-3rem)]">
        <div className="flex min-w-0 items-center gap-2 pe-8">
          <Eye aria-hidden className="size-4 shrink-0 text-signal" strokeWidth={1.5} />
          <DialogTitle className="truncate text-sm font-medium">
            {deviceName} · {screen.name}
          </DialogTitle>
        </div>
        <DialogDescription className="sr-only">
          A live picture of this screen. It can be looked at, but not clicked on.
        </DialogDescription>
        <div
          ref={setArea}
          className="flex min-h-0 flex-1 items-center justify-center overflow-hidden rounded-md bg-black"
        >
          {picture.kind === "picture" ? (
            <img
              src={picture.url}
              alt={`What's on ${screen.name} on ${deviceName}`}
              draggable={false}
              className="max-h-full max-w-full object-contain"
            />
          ) : picture.kind === "waiting" ? (
            <Loader2 aria-label="Waiting for a picture" className="size-5 animate-spin text-white/60" />
          ) : (
            <NoPicture why={picture.why} deviceName={deviceName} isThisPc={isThisPc} />
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

function NoPicture({
  why,
  deviceName,
  isThisPc,
}: {
  why: "notAllowed" | "needsUpdate" | "unavailable";
  deviceName: string;
  isThisPc: boolean;
}) {
  const messages = {
    notAllowed: {
      title: `${deviceName} isn't sharing its screens with this PC`,
      detail: `To allow it, open Hackerman on ${deviceName} and turn it on under Settings > Screens.`,
    },
    needsUpdate: {
      title: `${deviceName} can't show its screens yet`,
      detail: `Update Hackerman on ${deviceName} to this version.`,
    },
    unavailable: {
      title: isThisPc ? "This screen can't be shown right now" : `${deviceName} can't show this right now`,
      detail: "It may be locked or disabled, or showing something protected.",
    },
  };
  const { title, detail } = messages[why];
  return (
    <div className="flex max-w-md flex-col items-center gap-2 p-6 text-center text-white">
      <MonitorOff aria-hidden className="size-6 text-white/60" strokeWidth={1.5} />
      <p className="text-sm font-medium">{title}</p>
      <p className="text-xs text-white/70">{detail}</p>
    </div>
  );
}
