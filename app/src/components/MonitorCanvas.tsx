import {
  AppWindow,
  ArrowLeftRight,
  ArrowUpDown,
  Ban,
  Check,
  Download,
  FileUp,
  Maximize2,
  Move,
  Pause,
  Play,
  Plus,
  Scaling,
  SkipBack,
  SkipForward,
  Star,
} from "lucide-react";
import { createContext, useContext, useEffect, useMemo, useRef, useState } from "react";

import { DeviceIcon, NumberBadge, type Presence, ScreenStand } from "@/components/Art";
import { ScreenViewer } from "@/components/ScreenViewer";
import { Button } from "@/components/ui/button";
import {
  type Crossing,
  type DeviceRect,
  type Rect,
  type View,
  findCrossings,
  fitView,
  resizedWidth,
  resizeLimits,
  resolveOverlaps,
  screensChanged,
  snapDrag,
  snapResize,
  visibleCrossings,
} from "@/lib/canvas";
import { useAppearance } from "@/lib/appearance";
import { activeDevice, cantReceiveFilesBecause, deviceNumbers, isBuiltIn } from "@/lib/devices";
import { useFileDrag } from "@/lib/file-drag";
import { pictureSizeFor, useScreenPicture } from "@/lib/screen-pictures";
import type {
  AppView,
  DeviceView,
  Fingerprint,
  MediaCommand,
  ScreenView,
  UiState,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { send } from "@/store";

/** Space kept around the monitors, partly so there's room to drag them. */
const VIEW_PADDING = 56;
/** How close an edge gets before it sticks, in screen pixels. */
const SNAP_DISTANCE = 12;
const KEYBOARD_STEP = 20;
const KEYBOARD_BIG_STEP = 100;
const KEYBOARD_RESIZE_STEP = 40;
/** Below this many screen pixels, a gap is drawn as a line rather than a band. */
const THINNEST_BAND = 8;
const MOVES = "transition-[left,top,width,height] duration-200 ease-out motion-reduce:transition-none";
/** How long to say that the screens have changed. */
const SCREENS_CHANGED_FOR_MS = 10_000;
/** What a tile's name and details at the top and bottom leave of its height, in pixels. */
const TILE_TEXT_HEIGHT = 92;
/** A screen shorter than this shows its picture rather than its size and name at the bottom. */
const DETAILS_OVER_A_PICTURE_NEED_HEIGHT = 170;
/** What that leaves room for. */
const TILE_DETAILS_HEIGHT = 36;
const APP_ICON_WIDTH = 24;
const APP_ROW_HEIGHT = 24;
const PLAYER_ROW_HEIGHT = 30;
/** The padding of a tile, on both sides together, with a little to spare. */
const TILE_PADDING = 24;
/** Narrower than this, a player has room for its buttons but not its name. */
const PLAYER_NAME_NEEDS_WIDTH = 170;
/** An app's icon and three buttons, with their gaps and the border around them. */
const PLAYER_WIDTH_WITHOUT_NAME = 104;
/** How often the small pictures on the canvas change. */
const SMALL_PICTURE_EVERY_MS = 1000;
/** How far the pointer goes before pressing an app counts as dragging it, in pixels. */
const APP_DRAG_STARTS_AT = 5;

function keyOf(screen: { device: Fingerprint; monitor: string }): string {
  return `${screen.device}/${screen.monitor}`;
}

/** The width a screen is drawn at unless the user chooses another. */
function usualWidth(screen: ScreenView): number {
  return screen.pixelWidth / screen.scale;
}

/** The screen drawn at another width, keeping its shape. */
function withWidth(screen: ScreenView, width: number): ScreenView {
  const { min, max } = resizeLimits(usualWidth(screen));
  const allowed = Math.min(max, Math.max(min, width));
  return {
    ...screen,
    width: allowed,
    height: (allowed * screen.height) / screen.width,
    customWidth: allowed,
  };
}

/** Where to suggest another PC could go, when this one is on its own. */
function placeForAnotherPc(screens: ScreenView[]): DeviceRect {
  const main = screens.find((screen) => screen.primary) ?? screens[0];
  const right = Math.max(...screens.map((screen) => screen.x + screen.width));
  return {
    device: "another-pc",
    x: right + main.width * 0.45,
    y: main.y,
    width: main.width,
    height: main.height,
    online: false,
  };
}

/** What to say instead of the screen's name, for a PC that can't be reached. */
function awayReason(device: DeviceView | undefined): string {
  switch (device?.status) {
    case "off":
      return "Disabled";
    case "incompatible":
      return "Different version";
    default:
      return "Offline";
  }
}

interface DragStart {
  pointerStart: { x: number; y: number };
  /** The view doesn't refit during a drag, so nothing shifts under the pointer. */
  view: View;
}

interface Move extends DragStart {
  kind: "move";
  /** The monitors moving together. */
  group: Set<string>;
  dx: number;
  dy: number;
}

interface Resize extends DragStart {
  kind: "resize";
  monitor: string;
  width: number;
}

type Drag = Move | Resize;

/** An app being dragged towards another screen of its PC. */
interface AppDrag {
  app: AppView;
  from: ScreenView;
  x: number;
  y: number;
  /** The screen it's over, if it's over another one. */
  over: string | null;
}

/** Starts following the pointer when an app is pressed. */
const AppDragContext = createContext<
  (event: React.PointerEvent, app: AppView, from: ScreenView) => void
>(() => {});

export function MonitorCanvas({ state }: { state: UiState }) {
  const area = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [drag, setDrag] = useState<Drag | null>(null);
  const [movesOneScreen, setMovesOneScreen] = useState(false);
  // Where monitors were dropped, shown until the agent confirms the new layout.
  const [dropped, setDropped] = useState<Map<string, ScreenView> | null>(null);
  const { laptopKeyboards, monitorStands } = useAppearance();
  const fileDrag = useFileDrag();
  const [appDrag, setAppDrag] = useState<AppDrag | null>(null);
  const [viewing, setViewing] = useState<string | null>(null);

  useEffect(() => {
    const element = area.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => {
      setSize({ width: entry.contentRect.width, height: entry.contentRect.height });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => setDropped(null), [state.screens]);

  // Screens moving by themselves would be alarming with no word about why.
  const [screensNotice, setScreensNotice] = useState<string | null>(null);
  const screensBefore = useRef(state.screens);
  useEffect(() => {
    if (screensChanged(screensBefore.current, state.screens)) {
      setScreensNotice(
        state.layoutSaved
          ? "Your screens changed, so the layout saved for these screens is back."
          : "Your screens changed. The others kept their places, and you can drag any new one where you want it.",
      );
    }
    screensBefore.current = state.screens;
  }, [state.screens, state.layoutSaved]);
  useEffect(() => {
    if (screensNotice === null) return;
    const timer = setTimeout(() => setScreensNotice(null), SCREENS_CHANGED_FOR_MS);
    return () => clearTimeout(timer);
  }, [screensNotice]);

  const screens: ScreenView[] = state.screens.map((screen) => {
    const key = keyOf(screen);
    if (drag?.kind === "move" && drag.group.has(key)) {
      return { ...screen, x: screen.x + drag.dx, y: screen.y + drag.dy };
    }
    if (drag?.kind === "resize" && drag.monitor === key) {
      return withWidth(screen, drag.width);
    }
    return dropped?.get(key) ?? screen;
  });

  const alone = state.devices.every((device) => device.status === "available");
  const anotherPc = alone && screens.length > 0 ? placeForAnotherPc(screens) : null;
  const drawn: DeviceRect[] = anotherPc ? [...screens, anotherPc] : screens;

  const fitted = fitView(drawn, size.width, size.height, VIEW_PADDING);
  const view = drag?.view ?? fitted;
  const crossings = useMemo(() => findCrossings(drawn), [JSON.stringify(drawn)]);
  const shownCrossings = visibleCrossings(crossings);
  const isBand = (crossing: Crossing) => crossing.gap * view.scale >= THINNEST_BAND;
  const numbers = deviceNumbers(state);
  const active = activeDevice(state);
  const someHaveSeveral = new Set(state.screens.map((s) => s.device)).size < state.screens.length;

  function presenceOf(screen: ScreenView): Presence {
    if (!screen.online) return "away";
    return screen.device === active ? "active" : "present";
  }

  function groupFor(screen: ScreenView, altKey: boolean): Set<string> {
    const members =
      movesOneScreen !== altKey
        ? [screen]
        : state.screens.filter((other) => other.device === screen.device);
    return new Set(members.map(keyOf));
  }

  function partition(group: Set<string>) {
    return {
      moving: state.screens.filter((screen) => group.has(keyOf(screen))),
      others: screens.filter((screen) => !group.has(keyOf(screen))),
    };
  }

  /** Saves these screens as given, except that they never end up on top of another. */
  function save(changed: ScreenView[]) {
    const { others } = partition(new Set(changed.map(keyOf)));
    const push = resolveOverlaps(changed, others);
    const placed = changed.map((screen) => ({
      ...screen,
      x: screen.x + push.dx,
      y: screen.y + push.dy,
    }));
    setDropped(new Map(placed.map((screen) => [keyOf(screen), screen])));
    send({
      type: "setPlacements",
      placements: placed.map((screen) => ({
        device: screen.device,
        monitor: screen.monitor,
        x: screen.x,
        y: screen.y,
        width: screen.customWidth,
      })),
    });
  }

  function saveMove(group: Set<string>, dx: number, dy: number) {
    const { moving } = partition(group);
    save(moving.map((screen) => ({ ...screen, x: screen.x + dx, y: screen.y + dy })));
  }

  function resetSize(screen: ScreenView) {
    save([{ ...withWidth(screen, usualWidth(screen)), customWidth: null }]);
  }

  function dragStart(event: React.PointerEvent): DragStart {
    event.currentTarget.setPointerCapture(event.pointerId);
    return { pointerStart: { x: event.clientX, y: event.clientY }, view };
  }

  function onPointerDown(event: React.PointerEvent, screen: ScreenView) {
    if (event.button !== 0) return;
    setDrag({
      ...dragStart(event),
      kind: "move",
      group: groupFor(screen, event.altKey),
      dx: 0,
      dy: 0,
    });
  }

  function onResizeStart(event: React.PointerEvent, screen: ScreenView) {
    if (event.button !== 0) return;
    // Otherwise the tile underneath would start moving as well.
    event.stopPropagation();
    setDrag({ ...dragStart(event), kind: "resize", monitor: keyOf(screen), width: screen.width });
  }

  function onPointerMove(event: React.PointerEvent) {
    if (!drag) return;
    const rawDx = (event.clientX - drag.pointerStart.x) / drag.view.scale;
    const rawDy = (event.clientY - drag.pointerStart.y) / drag.view.scale;
    const threshold = SNAP_DISTANCE / drag.view.scale;
    if (drag.kind === "move") {
      const { moving, others } = partition(drag.group);
      setDrag({ ...drag, ...snapDrag(moving, others, rawDx, rawDy, threshold) });
      return;
    }
    const { moving, others } = partition(new Set([drag.monitor]));
    const screen = moving[0];
    if (!screen) return;
    const wanted = withWidth(screen, resizedWidth(screen, rawDx, rawDy)).width;
    setDrag({ ...drag, width: snapResize(screen, others, wanted, threshold) });
  }

  function onPointerUp() {
    if (!drag) return;
    if (drag.kind === "move") {
      if (drag.dx !== 0 || drag.dy !== 0) {
        saveMove(drag.group, drag.dx, drag.dy);
      }
    } else {
      const screen = partition(new Set([drag.monitor])).moving[0];
      if (screen && drag.width !== screen.width) {
        save([withWidth(screen, drag.width)]);
      }
    }
    setDrag(null);
  }

  function onKeyDown(event: React.KeyboardEvent, screen: ScreenView) {
    const step = event.shiftKey ? KEYBOARD_BIG_STEP : KEYBOARD_STEP;
    const moves: Record<string, [number, number]> = {
      ArrowLeft: [-step, 0],
      ArrowRight: [step, 0],
      ArrowUp: [0, -step],
      ArrowDown: [0, step],
    };
    const move = moves[event.key];
    if (move) {
      event.preventDefault();
      saveMove(groupFor(screen, event.altKey), move[0], move[1]);
    } else if (event.key === "+" || event.key === "=") {
      save([withWidth(screen, screen.width + KEYBOARD_RESIZE_STEP)]);
    } else if (event.key === "-") {
      save([withWidth(screen, screen.width - KEYBOARD_RESIZE_STEP)]);
    } else if (event.key === "0" && screen.customWidth !== null) {
      resetSize(screen);
    }
  }

  /** Clicking an app is handled by the button itself, unless the pointer goes somewhere first. */
  function onAppPressed(event: React.PointerEvent, app: AppView, from: ScreenView) {
    if (event.button !== 0) return;
    // Otherwise the screen underneath would start moving.
    event.stopPropagation();
    const start = { x: event.clientX, y: event.clientY };
    let dragging = false;
    const moved = (move: PointerEvent) => {
      if (!dragging) {
        if (Math.hypot(move.clientX - start.x, move.clientY - start.y) < APP_DRAG_STARTS_AT) return;
        dragging = true;
      }
      setAppDrag({
        app,
        from,
        x: move.clientX,
        y: move.clientY,
        over: screenKeyAt(move.clientX, move.clientY, keyOf(from)),
      });
    };
    const finish = (up: PointerEvent, cancelled: boolean) => {
      window.removeEventListener("pointermove", moved);
      window.removeEventListener("pointerup", released);
      window.removeEventListener("pointercancel", abandoned);
      if (!dragging) return;
      setAppDrag(null);
      // The click that comes with letting go isn't a click on the app. There's none at all if
      // the pointer left the window, and that mustn't take the next real one.
      const swallow = (click: Event) => click.stopPropagation();
      window.addEventListener("click", swallow, { capture: true, once: true });
      setTimeout(() => window.removeEventListener("click", swallow, { capture: true }), 50);
      const over = screenKeyAt(up.clientX, up.clientY, keyOf(from));
      const target = state.screens.find((screen) => keyOf(screen) === over);
      if (!cancelled && target && target.device === from.device) {
        send({
          type: "appAction",
          device: from.device,
          app: app.id,
          action: { moveTo: { from: from.monitor, to: target.monitor } },
        });
      }
    };
    const released = (up: PointerEvent) => finish(up, false);
    const abandoned = (up: PointerEvent) => finish(up, true);
    window.addEventListener("pointermove", moved);
    window.addEventListener("pointerup", released);
    window.addEventListener("pointercancel", abandoned);
  }

  const appDragTarget = appDrag && state.screens.find((screen) => keyOf(screen) === appDrag.over);
  const appDropAllowed = appDragTarget?.device === appDrag?.from.device;
  const screenBeingViewed = state.screens.find((screen) => keyOf(screen) === viewing);
  const nameOfDevice = (device: Fingerprint) =>
    device === state.me.fingerprint
      ? "This PC"
      : (state.devices.find((d) => d.fingerprint === device)?.name ?? "Unknown");

  const connected = state.devices.filter((device) => device.status === "connected");
  const unreachable = state.enabled
    ? connected.filter(
        (device) =>
          !crossings.some(
            (crossing) =>
              crossing.active &&
              !crossing.sameDevice &&
              crossing.devices.includes(device.fingerprint),
          ),
      )
    : [];

  return (
    <AppDragContext.Provider value={onAppPressed}>
    <div
      ref={area}
      className="relative min-h-0 flex-1 touch-none select-none overflow-hidden bg-background"
    >
      <div aria-hidden className="drawing-board pointer-events-none absolute inset-0" />

      {/* Underneath every screen, so that a screen placed below covers them. */}
      {screens
        .filter((screen) => (isBuiltIn(screen) ? laptopKeyboards : monitorStands))
        .map((screen) => (
          <div
            key={keyOf(screen)}
            aria-hidden
            data-presence={presenceOf(screen)}
            style={{
              left: screen.x * view.scale + view.offsetX,
              top: (screen.y + screen.height) * view.scale + view.offsetY,
              width: screen.width * view.scale,
            }}
            className={cn(
              "pointer-events-none absolute text-(--frame)",
              drag === null && MOVES,
              !screen.online && "opacity-60",
            )}
          >
            <ScreenStand
              laptop={isBuiltIn(screen)}
              width={screen.width * view.scale}
              height={screen.height * view.scale}
            />
          </div>
        ))}

      {anotherPc && <AnotherPc rect={anotherPc} view={view} />}

      {/* Bands go behind the screens, so one leading past an offline PC passes behind it. */}
      {shownCrossings.filter(isBand).map((crossing, index) => (
        <CrossingMarker
          key={`${crossing.orientation}-${crossing.x}-${crossing.y}-${crossing.gap}-${index}`}
          crossing={crossing}
          view={view}
          enabled={state.enabled}
          animate={drag === null}
        />
      ))}

      {screens.map((screen) => {
        const key = keyOf(screen);
        const device = state.devices.find((d) => d.fingerprint === screen.device);
        const isMe = screen.device === state.me.fingerprint;
        return (
          <MonitorTile
            key={key}
            screen={screen}
            view={view}
            number={numbers.get(screen.device) ?? 0}
            presence={presenceOf(screen)}
            deviceName={isMe ? "This PC" : (device?.name ?? "Unknown")}
            model={isMe ? state.me.model : (device?.model ?? null)}
            awayReason={awayReason(device)}
            draggingFiles={fileDrag.active}
            filesOver={fileDrag.over === screen.device}
            cantReceive={cantReceiveFilesBecause(state, screen.device)}
            dragging={
              drag?.kind === "move" ? drag.group.has(key) : drag?.kind === "resize" && drag.monitor === key
            }
            animate={drag === null}
            onPointerDown={(event) => onPointerDown(event, screen)}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onKeyDown={(event) => onKeyDown(event, screen)}
            onResizeStart={(event) => onResizeStart(event, screen)}
            onResetSize={() => resetSize(screen)}
            onView={() => setViewing(key)}
            appDrop={
              appDrag?.over === key ? (appDropAllowed ? "ok" : "blocked") : null
            }
          />
        );
      })}

      {/* Lines sit on the edges of the screens, so they go on top. */}
      {shownCrossings
        .filter((crossing) => !isBand(crossing))
        .map((crossing, index) => (
          <CrossingMarker
            key={`${crossing.orientation}-${crossing.x}-${crossing.y}-${crossing.gap}-${index}`}
            crossing={crossing}
            view={view}
            enabled={state.enabled}
            animate={drag === null}
          />
        ))}

      {someHaveSeveral && (
        <div
          role="group"
          aria-label="What dragging a screen moves"
          title="Hold Alt while dragging to do the opposite."
          className="absolute top-3 left-3 z-30 flex items-center gap-2"
        >
          <span className="eyebrow ps-1">Drag moves</span>
          <div className="flex items-center gap-0.5 rounded-lg border bg-card p-0.5">
            <Button
              size="xs"
              variant={movesOneScreen ? "ghost" : "secondary"}
              aria-pressed={!movesOneScreen}
              onClick={() => setMovesOneScreen(false)}
            >
              Whole PC
            </Button>
            <Button
              size="xs"
              variant={movesOneScreen ? "secondary" : "ghost"}
              aria-pressed={movesOneScreen}
              onClick={() => setMovesOneScreen(true)}
            >
              One screen
            </Button>
          </div>
        </div>
      )}

      {!alone && <LayoutStatus state={state} />}

      {fileDrag.active && (
        <p className="pointer-events-none absolute inset-x-4 bottom-3 z-30 mx-auto flex w-fit max-w-full items-center gap-1.5 rounded-lg border bg-card px-3 py-1.5 text-center text-xs font-medium">
          <FileUp aria-hidden className="size-3.5 shrink-0 text-signal" strokeWidth={1.5} />
          Drop on a screen to save on that PC
        </p>
      )}

      {screensNotice && !fileDrag.active && (
        <p
          role="status"
          className="pointer-events-none absolute inset-x-4 bottom-3 z-30 mx-auto w-fit max-w-full rounded-lg border bg-card px-3 py-1.5 text-center text-xs font-medium"
        >
          {screensNotice}
        </p>
      )}

      {unreachable.length > 0 && !fileDrag.active && !screensNotice && (
        <p className="pointer-events-none absolute inset-x-4 bottom-3 z-30 mx-auto w-fit max-w-full rounded-lg border bg-card px-3 py-1.5 text-center text-xs font-medium text-amber-700 dark:text-amber-400">
          Your cursor can't reach {unreachable.map((device) => device.name).join(" or ")} yet. Drag its
          screens until they line up with another PC's and a violet band appears.
        </p>
      )}

      {appDrag && (
        <>
          <div
            aria-hidden
            style={{ left: appDrag.x + 10, top: appDrag.y + 10 }}
            className="pointer-events-none fixed z-50 rounded-md border bg-card p-1 shadow-lg"
          >
            <AppIcon app={appDrag.app} />
          </div>
          <p
            role="status"
            className="pointer-events-none absolute inset-x-4 bottom-3 z-30 mx-auto w-fit max-w-full rounded-lg border bg-card px-3 py-1.5 text-center text-xs font-medium"
          >
            {appDrag.over === null
              ? `Drop ${appDrag.app.name} on another screen of ${nameOfDevice(appDrag.from.device)}`
              : appDropAllowed
                ? `Move ${appDrag.app.name} to this screen`
                : `${appDrag.app.name} can only move between the screens of ${nameOfDevice(appDrag.from.device)}`}
          </p>
        </>
      )}

      {screenBeingViewed && (
        <ScreenViewer
          screen={screenBeingViewed}
          deviceName={nameOfDevice(screenBeingViewed.device)}
          isThisPc={screenBeingViewed.device === state.me.fingerprint}
          onClose={() => setViewing(null)}
        />
      )}
    </div>
    </AppDragContext.Provider>
  );
}

/** The screen under this point on the canvas, leaving out the one an app is coming from. */
function screenKeyAt(x: number, y: number, leaving: string): string | null {
  const key = document
    .elementFromPoint(x, y)
    ?.closest("[data-screen-key]")
    ?.getAttribute("data-screen-key");
  return key && key !== leaving ? key : null;
}

/** Says whether this arrangement will come back if the screens change and change back. */
function LayoutStatus({ state }: { state: UiState }) {
  const saved = state.savedLayouts === 1 ? "1 is saved" : `${state.savedLayouts} are saved`;
  const help = `Each set of screens keeps its own layout, and it comes back with those screens. ${saved}. Moving a screen saves the layout for the screens there are now.`;
  return (
    <div title={help} className="absolute top-3 right-3 z-30 flex items-center gap-2">
      {state.layoutSaved ? (
        <span className="flex items-center gap-1 rounded-lg border bg-card px-2 py-1 text-xs font-medium">
          <Check aria-hidden className="size-3 text-signal" strokeWidth={2} />
          Layout saved
        </span>
      ) : (
        <>
          <span className="eyebrow">Not saved for these screens</span>
          <Button
            size="xs"
            variant="secondary"
            onClick={() => send({ type: "setPlacements", placements: [] })}
          >
            Save layout
          </Button>
        </>
      )}
    </div>
  );
}

function GuideItem({
  icon: Icon,
  title,
  children,
}: {
  icon: React.ComponentType<{ className?: string; strokeWidth?: number }>;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="min-w-0">
      <h3 className="flex items-center gap-1.5 text-xs font-medium">
        <Icon aria-hidden className="size-3.5 shrink-0" strokeWidth={1.5} />
        {title}
      </h3>
      <p className="mt-0.5 text-xs leading-snug text-muted-foreground">{children}</p>
    </div>
  );
}

/** How to use the canvas. Gives way when the window is too short to spare the room. */
export function CanvasGuide() {
  return (
    <div className="grid grid-cols-5 gap-4 border-t bg-card px-5 py-3 [@media(max-height:640px)]:hidden">
      <GuideItem icon={Move} title="Move">
        Drag a screen. Alt swaps Whole PC and One screen.
      </GuideItem>
      <GuideItem icon={Scaling} title="Resize">
        Drag its corner. Double-click it for the usual size.
      </GuideItem>
      <GuideItem icon={ArrowLeftRight} title="Cross">
        Violet bands are crossings. Screens needn't touch.
      </GuideItem>
      <GuideItem icon={FileUp} title="Send files">
        Drop files on a screen to save them on that PC.
      </GuideItem>
      <GuideItem icon={AppWindow} title="Apps">
        Click one to bring it up. Drag it to another screen of its PC.
      </GuideItem>
    </div>
  );
}

function pixelsOf(rect: Rect, view: View) {
  return {
    left: rect.x * view.scale + view.offsetX,
    top: rect.y * view.scale + view.offsetY,
    width: rect.width * view.scale,
    height: rect.height * view.scale,
  };
}

function AnotherPc({ rect, view }: { rect: Rect; view: View }) {
  return (
    <div
      aria-hidden
      data-presence="away"
      style={pixelsOf(rect, view)}
      className="@container pointer-events-none absolute flex flex-col items-center justify-center gap-1.5 rounded-[11px] border border-dashed border-(--frame) p-2 text-center"
    >
      <span className="flex size-6 items-center justify-center rounded-md border border-(--frame) bg-card text-muted-foreground">
        <Plus className="size-3.5" strokeWidth={1.5} />
      </span>
      <span className="eyebrow">Another PC</span>
    </div>
  );
}

interface MonitorTileProps {
  screen: ScreenView;
  view: View;
  number: number;
  presence: Presence;
  deviceName: string;
  model: string | null;
  awayReason: string;
  draggingFiles: boolean;
  filesOver: boolean;
  cantReceive: string | null;
  dragging: boolean;
  animate: boolean;
  onPointerDown: (event: React.PointerEvent) => void;
  onPointerMove: (event: React.PointerEvent) => void;
  onPointerUp: (event: React.PointerEvent) => void;
  onKeyDown: (event: React.KeyboardEvent) => void;
  onResizeStart: (event: React.PointerEvent) => void;
  onResetSize: () => void;
  onView: () => void;
  /** An app is being dragged over this screen, and whether it can go there. */
  appDrop: "ok" | "blocked" | null;
}

function MonitorTile({
  screen,
  view,
  number,
  presence,
  deviceName,
  model,
  awayReason,
  draggingFiles,
  filesOver,
  cantReceive,
  dragging,
  animate,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onKeyDown,
  onResizeStart,
  onResetSize,
  onView,
  appDrop,
}: MonitorTileProps) {
  const active = presence === "active";
  const away = presence === "away";
  const label = `${screen.name}, ${deviceName}${model ? `, ${model}` : ""}, PC number ${number}, ${screen.pixelWidth} by ${screen.pixelHeight}${active ? ", has the keyboard and mouse" : ""}${away ? `, ${awayReason}` : ""}. Use the arrow keys to move it, plus and minus to resize it, and Enter to see what's on it.`;
  const { screenPictures } = useAppearance();
  const canView = screenPictures && !away;
  // Each starts at a different moment, so that they don't all ask at once. It's chosen once, as
  // choosing again would start them all over.
  const [startAfter] = useState(() => Math.random() * SMALL_PICTURE_EVERY_MS);
  const pictureWidth = screen.width * view.scale;
  const pictureHeight = screen.height * view.scale;
  const size = pictureSizeFor(pictureWidth, pictureHeight, window.devicePixelRatio || 1);
  const picture = useScreenPicture(
    canView
      ? { device: screen.device, monitor: screen.monitor, ...size, cursor: false }
      : null,
    SMALL_PICTURE_EVERY_MS,
    startAfter,
  );
  const hasPicture = picture.kind === "picture";
  // Over a picture, the words need something behind them to be read at all.
  const onPicture = hasPicture && "rounded bg-card/85 px-1 py-0.5 backdrop-blur-sm";
  const hideDetails = hasPicture && pictureHeight < DETAILS_OVER_A_PICTURE_NEED_HEIGHT;
  return (
    <div
      role="button"
      tabIndex={0}
      aria-label={label}
      data-presence={presence}
      data-drop-device={screen.device}
      data-screen-key={keyOf(screen)}
      data-drop={filesOver ? (cantReceive ? "blocked" : "ok") : undefined}
      data-app-drop={appDrop ?? undefined}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onKeyDown={(event) => {
        if (event.key === "Enter" && canView) {
          onView();
        } else {
          onKeyDown(event);
        }
      }}
      onDoubleClick={() => {
        if (canView) onView();
      }}
      style={pixelsOf(screen, view)}
      className={cn(
        "frame absolute cursor-grab outline-none focus-visible:ring-3 focus-visible:ring-ring/50",
        animate && MOVES,
        active && "frame-glow",
        away && "border-dashed",
        dragging && "z-10 cursor-grabbing shadow-lg",
        draggingFiles && !filesOver && !cantReceive && "ring-1 ring-signal/50",
        filesOver && !cantReceive && "ring-2 ring-signal",
        filesOver && cantReceive && "ring-2 ring-destructive/60",
        appDrop === "ok" && "ring-2 ring-signal",
        appDrop === "blocked" && "ring-2 ring-destructive/60",
      )}
    >
      <div className={cn("frame-inner relative h-full overflow-hidden", away && "opacity-60")}>
        {hasPicture && (
          <img
            src={picture.url}
            alt=""
            data-screen-picture
            draggable={false}
            className="pointer-events-none absolute inset-0 size-full object-fill"
          />
        )}
        <div className="@container relative flex h-full flex-col justify-between gap-1 p-2 text-foreground">
          <div className={cn("flex min-w-0 items-start gap-2", canView && "pe-5")}>
            <DeviceIcon laptop={isBuiltIn(screen)} active={active} className="hidden @[11rem]:flex" />
            <div className={cn("min-w-0", onPicture)}>
              <div className="truncate text-xs font-medium">{deviceName}</div>
              {model && (
                <div
                  title={model}
                  className="hidden truncate text-[0.6875rem] text-muted-foreground @[6.5rem]:block"
                >
                  {model}
                </div>
              )}
            </div>
          </div>
          <AppsOnScreen
            screen={screen}
            view={view}
            deviceName={deviceName}
            extraRoom={hideDetails ? TILE_DETAILS_HEIGHT : 0}
          />
          <div
            className={cn(
              "eyebrow hidden w-fit max-w-full min-w-0 pe-3 @[6rem]:block",
              onPicture,
              hideDetails && "@[6rem]:hidden",
            )}
          >
            <div className="flex min-w-0 items-center gap-1">
              {screen.primary && (
                <Star aria-label="Main display" className="size-2 shrink-0 fill-current" />
              )}
              <span className="truncate">
                {away ? awayReason : isBuiltIn(screen) ? "Built-in" : screen.name}
              </span>
            </div>
            <div className="hidden truncate tracking-normal normal-case tabular-nums @[8.5rem]:block">
              {screen.pixelWidth} × {screen.pixelHeight}
              {screen.scale !== 1 && ` · ${Math.round(screen.scale * 100)}%`}
            </div>
          </div>
        </div>
      </div>
      {filesOver && <DropMessage name={deviceName} cantReceive={cantReceive} />}
      {canView && (
        <Button
          size="icon-xs"
          variant="secondary"
          title="See this screen larger"
          aria-label={`See ${screen.name} on ${deviceName} larger`}
          className="absolute top-1.5 right-3.5 z-[6] size-5 opacity-70 hover:opacity-100"
          onPointerDown={(event) => event.stopPropagation()}
          onDoubleClick={(event) => event.stopPropagation()}
          onKeyDown={(event) => event.stopPropagation()}
          onClick={onView}
        >
          <Maximize2 strokeWidth={1.5} />
        </Button>
      )}
      <NumberBadge
        number={number}
        active={active}
        className="absolute -top-2 -right-2 z-[25] shadow-xs"
      />
      <div
        data-resize-handle
        title="Drag to resize. Double-click for the usual size."
        onPointerDown={onResizeStart}
        onDoubleClick={(event) => {
          // Otherwise this would open the screen as well.
          event.stopPropagation();
          onResetSize();
        }}
        className="group/handle absolute right-0 bottom-0 size-4 cursor-nwse-resize"
      >
        <span
          aria-hidden
          className="absolute right-[0.3125rem] bottom-[0.3125rem] size-[0.4375rem] rounded-br-[2px] border-r border-b border-(--frame) group-hover/handle:border-foreground"
        />
      </div>
    </div>
  );
}

/** What's open on a screen, with buttons for anything that can be played. */
function AppsOnScreen({
  screen,
  view,
  deviceName,
  extraRoom,
}: {
  screen: ScreenView;
  view: View;
  deviceName: string;
  extraRoom: number;
}) {
  const { appsOnScreens, mediaButtons } = useAppearance();
  const width = screen.width * view.scale;
  const room = screen.height * view.scale - TILE_TEXT_HEIGHT + extraRoom;
  const players = mediaButtons ? screen.apps.filter((app) => app.media) : [];
  const others = appsOnScreens ? screen.apps.filter((app) => !players.includes(app)) : [];
  // The buttons come first. The apps get a row of their own if there's one left, and otherwise
  // whatever's left beside the last of the buttons.
  const shownPlayers = players.slice(0, Math.max(0, Math.floor(room / PLAYER_ROW_HEIGHT)));
  const rowLeft = room - shownPlayers.length * PLAYER_ROW_HEIGHT >= APP_ROW_HEIGHT;
  const besidePlayer = !rowLeft && shownPlayers.length > 0 && others.length > 0;
  const inner = width - TILE_PADDING;
  const showNames = inner >= PLAYER_NAME_NEEDS_WIDTH;
  const lastIndex = shownPlayers.length - 1;
  if (shownPlayers.length === 0 && (others.length === 0 || !rowLeft)) return null;

  return (
    <div className="flex min-h-0 flex-1 flex-col justify-center gap-1 overflow-hidden">
      {shownPlayers.map((app, index) => {
        const player = (
          <Player
            key={app.id}
            app={app}
            screen={screen}
            deviceName={deviceName}
            showName={showNames && !(besidePlayer && index === lastIndex)}
          />
        );
        return besidePlayer && index === lastIndex ? (
          <div key={app.id} className="flex min-w-0 items-center gap-1.5">
            {player}
            <AppIcons apps={others} screen={screen} deviceName={deviceName} room={inner - PLAYER_WIDTH_WITHOUT_NAME} />
          </div>
        ) : (
          player
        );
      })}
      {rowLeft && others.length > 0 && (
        <AppIcons apps={others} screen={screen} deviceName={deviceName} room={inner} />
      )}
    </div>
  );
}

function AppIcons({
  apps,
  screen,
  deviceName,
  room,
}: {
  apps: AppView[];
  screen: ScreenView;
  deviceName: string;
  room: number;
}) {
  const fit = Math.floor(room / APP_ICON_WIDTH);
  if (fit < 1) return null;
  const shown = apps.length > fit ? apps.slice(0, fit - 1) : apps;
  const rest = apps.slice(shown.length);
  return (
    <ul aria-label="Open apps" className="flex min-w-0 items-center gap-1">
      {shown.map((app) => (
        <li key={app.id} title={app.minimized ? `${app.name} (minimized)` : app.name}>
          <AppButton app={app} screen={screen} deviceName={deviceName} />
        </li>
      ))}
      {rest.length > 0 && (
        <li
          title={rest.map((app) => app.name).join(", ")}
          className="eyebrow flex size-5 items-center justify-center tracking-normal tabular-nums"
        >
          +{rest.length}
        </li>
      )}
    </ul>
  );
}

/** An app's icon, to click to bring it up, or to drag to another of its PC's screens. */
function AppButton({
  app,
  screen,
  deviceName,
}: {
  app: AppView;
  screen: ScreenView;
  deviceName: string;
}) {
  const startDragging = useContext(AppDragContext);
  // Something playing from the tray has no window to do this with.
  if (!app.hasWindow) return <AppIcon app={app} />;
  return (
    <button
      type="button"
      data-app-button={app.id}
      aria-label={`Bring ${app.name} to the front on ${deviceName}. Drag it to move it to another screen.`}
      onPointerDown={(event) => startDragging(event, app, screen)}
      onDoubleClick={(event) => event.stopPropagation()}
      onKeyDown={(event) => event.stopPropagation()}
      onClick={() =>
        send({
          type: "appAction",
          device: screen.device,
          app: app.id,
          action: { bringUp: { monitor: screen.monitor } },
        })
      }
      className="block shrink-0 cursor-pointer rounded-[3px] outline-none transition-transform hover:scale-115 focus-visible:ring-2 focus-visible:ring-ring/70 active:cursor-grabbing motion-reduce:transition-none"
    >
      <AppIcon app={app} />
    </button>
  );
}

function AppIcon({ app }: { app: AppView }) {
  const dimmed = app.minimized && "opacity-45";
  if (app.icon) {
    return (
      <img
        src={app.icon}
        alt={app.name}
        draggable={false}
        className={cn("size-5 shrink-0 rounded-[3px] object-contain", dimmed)}
      />
    );
  }
  return (
    <span
      role="img"
      aria-label={app.name}
      className={cn(
        "flex size-5 shrink-0 items-center justify-center rounded-[3px] border bg-muted text-[0.625rem] font-medium uppercase",
        dimmed,
      )}
    >
      {app.name.charAt(0)}
    </span>
  );
}

function Player({
  app,
  screen,
  deviceName,
  showName,
}: {
  app: AppView;
  screen: ScreenView;
  deviceName: string;
  showName: boolean;
}) {
  const media = app.media;
  if (!media) return null;
  const run = (command: MediaCommand) =>
    send({ type: "mediaCommand", device: screen.device, app: app.id, command });
  const where = `${app.name} on ${deviceName}`;
  const playing = media.playing === true;
  return (
    // The buttons are for pressing, so nothing here starts moving the screen.
    <div
      data-player={app.id}
      onPointerDown={(event) => event.stopPropagation()}
      onDoubleClick={(event) => event.stopPropagation()}
      onKeyDown={(event) => event.stopPropagation()}
      className="flex w-fit max-w-full min-w-0 cursor-default items-center gap-1 rounded-lg border bg-card py-0.5 ps-1 pe-0.5"
    >
      <span title={app.name} className="shrink-0">
        <AppButton app={app} screen={screen} deviceName={deviceName} />
      </span>
      {showName && (
        <span title={app.name} className="min-w-0 truncate pe-0.5 text-[0.6875rem] font-medium">
          {app.name}
        </span>
      )}
      <Button
        size="icon-xs"
        variant="ghost"
        className="size-5"
        title="Back"
        aria-label={`Back on ${where}`}
        disabled={!media.canGoBack}
        onClick={() => run("back")}
      >
        <SkipBack strokeWidth={1.5} />
      </Button>
      <Button
        size="icon-xs"
        variant="ghost"
        className={cn("size-5", playing && "text-signal")}
        title={media.playing === null ? "Play or pause" : playing ? "Pause" : "Play"}
        aria-label={`${media.playing === null ? "Play or pause" : playing ? "Pause" : "Play"} ${where}`}
        onClick={() => run("playPause")}
      >
        {playing ? <Pause strokeWidth={1.5} /> : <Play strokeWidth={1.5} />}
      </Button>
      <Button
        size="icon-xs"
        variant="ghost"
        className="size-5"
        title="Forward"
        aria-label={`Forward on ${where}`}
        disabled={!media.canGoForward}
        onClick={() => run("forward")}
      >
        <SkipForward strokeWidth={1.5} />
      </Button>
    </div>
  );
}

/** Says what dropping files here will do, over the screen they're being held above. */
function DropMessage({ name, cantReceive }: { name: string; cantReceive: string | null }) {
  const Icon = cantReceive ? Ban : Download;
  return (
    <div className="@container pointer-events-none absolute inset-[3px] z-[5] flex flex-col items-center justify-center gap-1 rounded-[7px] bg-card p-1 text-center">
      <Icon
        aria-hidden
        className={cn("size-4 shrink-0", cantReceive ? "text-destructive" : "text-signal")}
        strokeWidth={1.5}
      />
      <span className="hidden text-[0.6875rem] leading-tight font-medium @[6rem]:block">
        {cantReceive ?? `Save on ${name}`}
      </span>
    </div>
  );
}

function CrossingMarker({
  crossing,
  view,
  enabled,
  animate,
}: {
  crossing: Crossing;
  view: View;
  enabled: boolean;
  animate: boolean;
}) {
  const vertical = crossing.orientation === "vertical";
  const left = crossing.x * view.scale + view.offsetX;
  const top = crossing.y * view.scale + view.offsetY;
  const length = crossing.length * view.scale;
  const across = crossing.gap * view.scale;
  const isBand = across >= THINNEST_BAND;
  // Bands fill the gap. Otherwise, a line of this thickness sits in the middle of it.
  const thickness = isBand ? across : 2;
  const start = isBand ? 0 : across / 2 - thickness / 2;
  const Icon = vertical ? ArrowLeftRight : ArrowUpDown;
  const tone = !crossing.active || !enabled ? "muted" : crossing.sameDevice ? "neutral" : "live";
  return (
    <div
      aria-hidden
      data-crossing={tone}
      style={
        vertical
          ? { left: left + start, top, width: thickness, height: length }
          : { left, top: top + start, width: length, height: thickness }
      }
      className={cn(
        "pointer-events-none absolute",
        isBand
          ? vertical
            ? "crossing-band border-x"
            : "crossing-band border-y"
          : "z-20 rounded-full bg-(--cross)",
        animate && MOVES,
      )}
    >
      {isBand && length > 12 && (
        <>
          {/* A straight line from edge to edge, with a dot where it meets each screen. */}
          <span
            className={cn(
              "absolute bg-(--cross)",
              vertical ? "inset-x-0 top-1/2 h-px" : "inset-y-0 left-1/2 w-px",
            )}
          />
          {[0, 1].map((end) => (
            <span
              key={end}
              style={vertical ? { left: `${end * 100}%`, top: "50%" } : { left: "50%", top: `${end * 100}%` }}
              className="absolute size-[0.3125rem] -translate-x-1/2 -translate-y-1/2 rounded-full bg-(--cross)"
            />
          ))}
        </>
      )}
      {length > 28 && (
        <span className="absolute top-1/2 left-1/2 flex size-[1.125rem] -translate-x-1/2 -translate-y-1/2 items-center justify-center rounded-md border border-(--cross) bg-card text-(--cross)">
          <Icon className="size-2.5" strokeWidth={2} />
        </span>
      )}
    </div>
  );
}
