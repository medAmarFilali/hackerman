import { getCurrentWebview } from "@tauri-apps/api/webview";
import { create } from "zustand";

import { cantReceiveFilesBecause } from "@/lib/devices";
import type { Fingerprint } from "@/lib/types";
import { send, useApp } from "@/store";

interface FileDrag {
  /** Files from another program are being dragged over this window. */
  active: boolean;
  /** The PC whose screen or card they're over. */
  over: Fingerprint | null;
}

export const useFileDrag = create<FileDrag>(() => ({ active: false, over: null }));

/** Which PC this spot on the window stands for, if any. Windows gives physical pixels. */
function deviceAt(position: { x: number; y: number }): Fingerprint | null {
  const x = position.x / window.devicePixelRatio;
  const y = position.y / window.devicePixelRatio;
  const element = document.elementFromPoint(x, y)?.closest("[data-drop-device]");
  return element?.getAttribute("data-drop-device") ?? null;
}

/** Call once, when the app starts. */
export function startFileDrag(): void {
  void getCurrentWebview().onDragDropEvent(({ payload }) => {
    switch (payload.type) {
      case "enter":
      case "over":
        useFileDrag.setState({ active: true, over: deviceAt(payload.position) });
        break;
      case "drop": {
        const device = deviceAt(payload.position);
        const { state } = useApp.getState();
        useFileDrag.setState({ active: false, over: null });
        if (device && state && !cantReceiveFilesBecause(state, device)) {
          send({ type: "sendFiles", device, paths: payload.paths });
        }
        break;
      }
      case "leave":
        useFileDrag.setState({ active: false, over: null });
        break;
    }
  });
}
