import { open as chooseFolder } from "@tauri-apps/plugin-dialog";
import { disable as stopAutostart, enable as startAutostart, isEnabled } from "@tauri-apps/plugin-autostart";
import {
  Eye,
  FolderOpen,
  Info,
  Keyboard,
  MonitorCog,
  MousePointer2,
  Music,
  Palette,
  Paperclip,
  Settings,
  X,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useId, useState } from "react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { changeAppearance, useAppearance } from "@/lib/appearance";
import {
  NO_SHORTCUT,
  USUAL_MEDIA_SHORTCUTS,
  describeShortcut,
  isModifierKey,
  shortcutFromEvent,
  shortcutProblem,
  useKeyLabels,
} from "@/lib/shortcuts";
import { type ThemeChoice, chooseTheme, useTheme } from "@/lib/theme";
import type {
  CrossModifier,
  LockScreenStatus,
  MediaShortcuts,
  NameClash,
  ReceiveFiles,
  Rules,
  Shortcut,
  UiState,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { lockScreenStatus, openFolder, openLogFolder, send, setLockScreen } from "@/store";

const EDGE_DELAYS = [
  { value: "0", label: "Right away" },
  { value: "150", label: "After a short push" },
  { value: "300", label: "After a medium push" },
  { value: "600", label: "After a long push" },
];

const CROSS_KEYS: { value: CrossModifier; label: string }[] = [
  { value: "none", label: "No key needed" },
  { value: "ctrl", label: "While holding Ctrl" },
  { value: "shift", label: "While holding Shift" },
  { value: "alt", label: "While holding Alt" },
];

const CORNER_GUARDS = [
  { value: "0", label: "Cross anywhere" },
  { value: "25", label: "Small corners" },
  { value: "50", label: "Medium corners" },
  { value: "100", label: "Large corners" },
];

const RECEIVING: { value: ReceiveFiles; label: string }[] = [
  { value: "askFirst", label: "Ask the first time" },
  { value: "ask", label: "Ask every time" },
  { value: "anyDevice", label: "Save them straight away" },
  { value: "never", label: "Turn them away" },
];

const NAME_CLASHES: { value: NameClash; label: string }[] = [
  { value: "keepBoth", label: "Keep both" },
  { value: "replace", label: "Replace it" },
  { value: "skip", label: "Keep the one that's there" },
];

const THEMES: { value: ThemeChoice; label: string }[] = [
  { value: "light", label: "Light" },
  { value: "dark", label: "Dark" },
  { value: "system", label: "Same as Windows" },
];

type SectionId =
  | "general"
  | "cursor"
  | "files"
  | "media"
  | "screens"
  | "appearance"
  | "shortcuts"
  | "about";

const SECTIONS: { id: SectionId; label: string; icon: LucideIcon }[] = [
  { id: "general", label: "General", icon: MonitorCog },
  { id: "cursor", label: "Cursor", icon: MousePointer2 },
  { id: "files", label: "Clipboard & files", icon: Paperclip },
  { id: "media", label: "Apps & media", icon: Music },
  { id: "screens", label: "Screens", icon: Eye },
  { id: "appearance", label: "Appearance", icon: Palette },
  { id: "shortcuts", label: "Shortcuts", icon: Keyboard },
  { id: "about", label: "About", icon: Info },
];

export function SettingsDialog({ state }: { state: UiState }) {
  const [open, setOpen] = useState(false);
  const [section, setSection] = useState<SectionId>("general");
  const { rules } = state;
  const update = (change: Partial<Rules>) =>
    send({ type: "setRules", rules: { ...rules, ...change } });
  const heading = SECTIONS.find((candidate) => candidate.id === section)?.label;

  return (
    <>
      <Tooltip>
        <TooltipTrigger asChild>
          <Button size="icon-sm" variant="ghost" aria-label="Settings" onClick={() => setOpen(true)}>
            <Settings />
          </Button>
        </TooltipTrigger>
        <TooltipContent>Settings</TooltipContent>
      </Tooltip>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="flex h-[min(38rem,calc(100vh-2rem))] gap-0 overflow-hidden p-0 sm:max-w-3xl">
          <nav aria-label="Settings sections" className="flex w-48 shrink-0 flex-col gap-0.5 border-r bg-muted/40 p-3">
            <DialogTitle className="px-2 pt-1 pb-3">Settings</DialogTitle>
            <DialogDescription className="sr-only">These apply to this PC only.</DialogDescription>
            {SECTIONS.map(({ id, label, icon: Icon }) => (
              <button
                key={id}
                type="button"
                aria-current={section === id ? "page" : undefined}
                onClick={() => setSection(id)}
                className={cn(
                  "flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm outline-none transition-colors",
                  "hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50",
                  section === id
                    ? "bg-secondary font-medium text-secondary-foreground hover:bg-secondary"
                    : "text-muted-foreground",
                )}
              >
                <Icon aria-hidden className="size-4 shrink-0" strokeWidth={1.5} />
                <span className="truncate">{label}</span>
              </button>
            ))}
            <p className="eyebrow mt-auto px-2 pb-1 tracking-normal normal-case">
              These apply to this PC only.
            </p>
          </nav>

          <div className="flex min-w-0 flex-1 flex-col">
            <h3 className="border-b px-5 py-3.5 pe-12 text-sm font-medium">{heading}</h3>
            <ScrollArea className="min-h-0 flex-1">
              <div className="flex flex-col divide-y px-5 [&>*]:py-3.5">
                {section === "general" && <General state={state} open={open} />}
                {section === "cursor" && <Cursor rules={rules} update={update} />}
                {section === "files" && <Files state={state} update={update} />}
                {section === "media" && <AppsAndMedia rules={rules} update={update} />}
                {section === "screens" && <Screens state={state} />}
                {section === "appearance" && <Appearance />}
                {section === "shortcuts" && <Shortcuts state={state} update={update} />}
                {section === "about" && <About state={state} />}
              </div>
            </ScrollArea>
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}

function Row({
  id,
  label,
  description,
  children,
}: {
  id?: string;
  label: string;
  description?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-start justify-between gap-6">
      <div className="flex min-w-0 flex-col gap-0.5">
        <Label htmlFor={id} className="text-sm font-medium">
          {label}
        </Label>
        {description && <div className="text-xs text-muted-foreground">{description}</div>}
      </div>
      <div className="flex shrink-0 items-center gap-2">{children}</div>
    </div>
  );
}

function SwitchRow({
  label,
  description,
  checked,
  disabled,
  onChange,
}: {
  label: string;
  description: React.ReactNode;
  checked: boolean;
  disabled?: boolean;
  onChange: (checked: boolean) => void;
}) {
  const id = useId();
  return (
    <Row id={id} label={label} description={description}>
      <Switch
        id={id}
        checked={checked}
        disabled={disabled}
        onCheckedChange={onChange}
        className="mt-0.5 data-checked:bg-signal"
      />
    </Row>
  );
}

function SelectRow<T extends string>({
  label,
  description,
  value,
  options,
  onChange,
}: {
  label: string;
  description: React.ReactNode;
  value: T;
  options: { value: T; label: string }[];
  onChange: (value: T) => void;
}) {
  const id = useId();
  return (
    <Row id={id} label={label} description={description}>
      <Select value={value} onValueChange={(chosen) => onChange(chosen as T)}>
        <SelectTrigger id={id} className="w-52">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => (
            <SelectItem key={option.value} value={option.value}>
              {option.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </Row>
  );
}

function General({ state, open }: { state: UiState; open: boolean }) {
  const nameId = useId();
  const [name, setName] = useState(state.me.name);
  // Whether this starts with Windows, once that's known. Something else can change it.
  const [autostart, setAutostart] = useState<boolean | null>(null);

  useEffect(() => setName(state.me.name), [state.me.name]);
  useEffect(() => {
    if (open) {
      isEnabled()
        .then(setAutostart)
        .catch(() => setAutostart(null));
    }
  }, [open]);

  function saveName() {
    const trimmed = name.trim();
    if (trimmed && trimmed !== state.me.name) {
      send({ type: "rename", name: trimmed });
    } else {
      setName(state.me.name);
    }
  }

  async function changeAutostart(on: boolean) {
    try {
      await (on ? startAutostart() : stopAutostart());
      setAutostart(on);
    } catch {
      setAutostart(await isEnabled().catch(() => null));
    }
  }

  return (
    <>
      <Row id={nameId} label="Name of this PC" description="How it appears on your other PCs.">
        <Input
          id={nameId}
          className="w-52"
          value={name}
          maxLength={40}
          onChange={(event) => setName(event.target.value)}
          onBlur={saveName}
          onKeyDown={(event) => event.key === "Enter" && event.currentTarget.blur()}
        />
      </Row>
      <SwitchRow
        label="Share with other PCs"
        description="Off is the same as the Disable button: this PC keeps its own keyboard and mouse, and nothing is shared."
        checked={state.enabled}
        onChange={(enabled) => send({ type: "setEnabled", enabled })}
      />
      <SwitchRow
        label="Start with Windows"
        description={
          autostart === null
            ? "Windows wouldn't say whether this is on."
            : "Starts when you log in, hidden in the tray and ready to use."
        }
        checked={autostart ?? false}
        disabled={autostart === null}
        onChange={(on) => void changeAutostart(on)}
      />
      <LockScreen open={open} />
    </>
  );
}

function LockScreen({ open }: { open: boolean }) {
  const [status, setStatus] = useState<LockScreenStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const refresh = () =>
    lockScreenStatus()
      .then(setStatus)
      .catch(() => setStatus(null));

  useEffect(() => {
    if (open) void refresh();
  }, [open]);

  async function change(on: boolean) {
    setBusy(true);
    setNote(null);
    try {
      const happened = await setLockScreen(on);
      if (!happened) setNote("Windows wasn't given permission, so nothing changed.");
    } catch (problem) {
      setNote(String(problem));
    }
    await refresh();
    setBusy(false);
  }

  if (!status?.available) return null;
  const outOfDate = status.on && !status.upToDate;
  const notRunning = status.on && !status.running && !outOfDate;
  const toAllow = status.on && !outOfDate ? status.notAllowedYet : [];

  return (
    <div className="flex flex-col gap-2">
      <SwitchRow
        label="Work at the lock screen"
        description="Lets the keyboard and mouse of your paired PCs reach this PC's lock screen and sign-in screen, even right after a restart, so you can type your password from another PC. Anyone using one of those PCs could do that too. PCs you pair later have to be allowed here. The clipboard and files aren't shared while it's locked. Windows asks for administrator permission."
        checked={status.on}
        disabled={busy}
        onChange={(on) => void change(on)}
      />
      {outOfDate && (
        <div className="flex items-center justify-between gap-3 rounded-md border px-2.5 py-1.5">
          <span className="text-xs text-muted-foreground">
            It's still from an older version of Hackerman.
          </span>
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void change(true)}>
            Update it
          </Button>
        </div>
      )}
      {toAllow.length > 0 && (
        <div className="flex items-center justify-between gap-3 rounded-md border px-2.5 py-1.5">
          <span className="min-w-0 text-xs text-muted-foreground">
            {toAllow.join(" and ")} can't reach this lock screen until you allow{" "}
            {toAllow.length === 1 ? "it" : "them"}. Only do that if you recognise{" "}
            {toAllow.length === 1 ? "it" : "them"}.
          </span>
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void change(true)}>
            Allow
          </Button>
        </div>
      )}
      {notRunning && (
        <p className="text-xs text-amber-700 dark:text-amber-400">
          It's set up, but isn't running. Turning it off and on again may fix that.
        </p>
      )}
      {note && (
        <p role="alert" className="text-xs text-amber-700 dark:text-amber-400">
          {note}
        </p>
      )}
    </div>
  );
}

function Cursor({
  rules,
  update,
}: {
  rules: Rules;
  update: (change: Partial<Rules>) => void;
}) {
  return (
    <>
      <SelectRow
        label="Cross to another PC"
        description="A push against the edge helps avoid crossing by accident when reaching for something near it."
        value={String(rules.edgeDelayMs)}
        options={EDGE_DELAYS}
        onChange={(value) => update({ edgeDelayMs: Number(value) })}
      />
      <SelectRow
        label="Hold a key to cross"
        description="The cursor stops at the edge unless this key is down. Moving between this PC's own screens isn't affected."
        value={rules.crossModifier}
        options={CROSS_KEYS}
        onChange={(crossModifier) => update({ crossModifier })}
      />
      <SelectRow
        label="Keep the corners here"
        description="The cursor doesn't cross near a corner of the screen, so that reaching for the Start button or a close button stays on this PC."
        value={String(rules.cornerGuard)}
        options={CORNER_GUARDS}
        onChange={(value) => update({ cornerGuard: Number(value) })}
      />
      <SwitchRow
        label="Stay here during full-screen apps"
        description="Keeps the cursor on this PC while a game or other full-screen app is in front."
        checked={rules.stayInFullscreen}
        onChange={(stayInFullscreen) => update({ stayInFullscreen })}
      />
    </>
  );
}

function Files({
  state,
  update,
}: {
  state: UiState;
  update: (change: Partial<Rules>) => void;
}) {
  const { rules } = state;
  const usingDesktop = !rules.dropFolder;
  const folder = rules.dropFolder ?? state.desktopFolder;

  async function browse() {
    const chosen = await chooseFolder({
      directory: true,
      defaultPath: folder ?? undefined,
      title: "Where to save files from other PCs",
    });
    if (typeof chosen === "string") {
      update({ dropFolder: chosen });
    }
  }

  return (
    <>
      <SwitchRow
        label="Share the clipboard"
        description="What you copy can be pasted on the PC your cursor moves to, including files of any size. Passwords copied from a password manager are never shared."
        checked={rules.clipboard}
        onChange={(clipboard) => update({ clipboard })}
      />
      <SelectRow
        label="Files sent from other PCs"
        description="What happens when someone drops files on one of this PC's screens. Asking the first time asks about each PC until you always allow it."
        value={rules.receiveFiles}
        options={RECEIVING}
        onChange={(receiveFiles) => update({ receiveFiles })}
      />
      {rules.receiveFiles === "askFirst" && <AllowedPcs state={state} />}
      <SwitchRow
        label="Tell me when files arrive"
        description="Shows a Windows notification with a button to open the folder, unless this window is in front."
        checked={rules.notifyFiles}
        onChange={(notifyFiles) => update({ notifyFiles })}
      />
      <div className="flex flex-col gap-2">
        <div className="flex flex-col gap-0.5">
          <span className="text-sm font-medium">Save them in</span>
          <span className="text-xs text-muted-foreground">
            {usingDesktop
              ? "The Desktop, unless you choose another folder. It's created if it isn't there."
              : "It's created if it isn't there."}
          </span>
        </div>
        <p
          title={folder ?? undefined}
          className="truncate rounded-md border bg-background px-2.5 py-1.5 font-mono text-xs select-text"
        >
          {folder ?? "No folder chosen"}
        </p>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="outline" onClick={() => void browse()}>
            Choose a folder…
          </Button>
          {!usingDesktop && (
            <Button size="sm" variant="ghost" onClick={() => update({ dropFolder: null })}>
              Use the Desktop
            </Button>
          )}
          {folder && (
            <Button size="sm" variant="ghost" onClick={() => openFolder(folder)}>
              <FolderOpen aria-hidden />
              Open it
            </Button>
          )}
        </div>
      </div>
      <SelectRow
        label="If a file with that name is there"
        description="Keeping both names the new one like report (1).pdf."
        value={rules.nameClash}
        options={NAME_CLASHES}
        onChange={(nameClash) => update({ nameClash })}
      />
    </>
  );
}

function AllowedPcs({ state }: { state: UiState }) {
  const allowed = state.devices.filter((device) => device.filesAllowed);
  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-col gap-0.5">
        <span className="text-sm font-medium">PCs that don't need to ask</span>
        <span className="text-xs text-muted-foreground">
          {allowed.length === 0
            ? "None yet. You choose this when a PC first sends you files."
            : "These can send files here whenever they like."}
        </span>
      </div>
      {allowed.map((device) => (
        <div
          key={device.fingerprint}
          className="flex items-center justify-between gap-3 rounded-md border px-2.5 py-1.5"
        >
          <span title={device.name} className="min-w-0 truncate text-xs font-medium">
            {device.name}
          </span>
          <Button
            size="xs"
            variant="ghost"
            aria-label={`Stop allowing ${device.name}`}
            onClick={() =>
              send({ type: "setFilesAllowed", device: device.fingerprint, allowed: false })
            }
          >
            Stop allowing
          </Button>
        </div>
      ))}
    </div>
  );
}

function AppsAndMedia({
  rules,
  update,
}: {
  rules: Rules;
  update: (change: Partial<Rules>) => void;
}) {
  const { appsOnScreens, mediaButtons } = useAppearance();
  return (
    <>
      <SwitchRow
        label="Show apps on the screens"
        description="Draws the icons of the apps open on each screen on the canvas."
        checked={appsOnScreens}
        onChange={(on) => changeAppearance({ appsOnScreens: on })}
      />
      <SwitchRow
        label="Show media buttons"
        description="Puts back, pause and forward on the screens where something like Spotify or Stremio is open."
        checked={mediaButtons}
        onChange={(on) => changeAppearance({ mediaButtons: on })}
      />
      <SwitchRow
        label="Let other PCs see what's open here"
        description="Sends the names and icons of the apps open on this PC to your paired PCs. Window titles, and what's in the windows, are never sent."
        checked={rules.shareApps}
        onChange={(shareApps) => update({ shareApps })}
      />
      <SwitchRow
        label="Let other PCs control media here"
        description="Lets your paired PCs play, pause and skip what's playing on this PC."
        checked={rules.mediaControl}
        onChange={(mediaControl) => update({ mediaControl })}
      />
    </>
  );
}

function Screens({ state }: { state: UiState }) {
  const { screenPictures } = useAppearance();
  const paired = state.devices.filter((device) => device.status !== "available");
  return (
    <>
      <SwitchRow
        label="Show what's on the screens"
        description="Shows a live picture of each screen on the canvas, for this PC and the PCs that allow it. Pictures are only taken while this window is open."
        checked={screenPictures}
        onChange={(on) => changeAppearance({ screenPictures: on })}
      />
      <div className="flex flex-col gap-2">
        <div className="flex flex-col gap-0.5">
          <span className="text-sm font-medium">PCs that can see this PC's screens</span>
          <span className="text-xs text-muted-foreground">
            {paired.length === 0
              ? "Pair with another PC first."
              : "Anything on the screens can be seen, including passwords being typed in plain text and private messages. Hackerman says so when one of them is looking. Nothing can be seen while this PC is locked."}
          </span>
        </div>
        {paired.map((device) => (
          <ScreenPermission key={device.fingerprint} device={device} />
        ))}
      </div>
    </>
  );
}

function ScreenPermission({ device }: { device: UiState["devices"][number] }) {
  const id = useId();
  return (
    <div className="flex items-center justify-between gap-3 rounded-md border px-2.5 py-1.5">
      <Label htmlFor={id} title={device.name} className="min-w-0 truncate text-xs font-medium">
        {device.name}
      </Label>
      <Switch
        id={id}
        aria-label={`Let ${device.name} see this PC's screens`}
        checked={device.screensAllowed}
        onCheckedChange={(allowed) =>
          send({ type: "setScreensAllowed", device: device.fingerprint, allowed })
        }
        className="data-checked:bg-signal"
      />
    </div>
  );
}

function Appearance() {
  const { laptopKeyboards, monitorStands } = useAppearance();
  const choice = useTheme((store) => store.choice);
  return (
    <>
      <SelectRow
        label="Theme"
        description="The switch at the top of the window changes this too."
        value={choice}
        options={THEMES}
        onChange={chooseTheme}
      />
      <SwitchRow
        label="Laptop keyboards"
        description="Draws a small keyboard under each laptop's screen on the canvas."
        checked={laptopKeyboards}
        onChange={(on) => changeAppearance({ laptopKeyboards: on })}
      />
      <SwitchRow
        label="Monitor stands"
        description="Draws a stand under each monitor on the canvas."
        checked={monitorStands}
        onChange={(on) => changeAppearance({ monitorStands: on })}
      />
    </>
  );
}

function Key({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="rounded border bg-background px-1 py-px font-mono text-[0.6875rem] whitespace-nowrap">
      {children}
    </kbd>
  );
}

function Shortcuts({
  state,
  update,
}: {
  state: UiState;
  update: (change: Partial<Rules>) => void;
}) {
  const paired = state.devices.filter((device) => device.status !== "available");
  return (
    <>
      <SwitchRow
        label="Keyboard shortcuts"
        description="Turn these off if they clash with another program. Ctrl+Alt+Home always works."
        checked={state.rules.shortcuts}
        onChange={(shortcuts) => update({ shortcuts })}
      />
      <div className={cn(!state.rules.shortcuts && "opacity-50")}>
        <p className="mb-2 text-xs text-muted-foreground">
          The number on a PC's screens is its shortcut. Use the left Alt key: AltGr stays free for
          typing characters.
        </p>
        <dl className="grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-1.5 text-xs">
          <dt>
            <Key>Ctrl+Alt+L</Key>
          </dt>
          <dd className="text-muted-foreground">Keep the cursor on the PC it's on now, or stop</dd>
          <dt>
            <Key>Ctrl+Alt+1</Key>
          </dt>
          <dd className="text-muted-foreground">Jump to this PC</dd>
          {paired.slice(0, 8).map((device, index) => (
            <div key={device.fingerprint} className="contents">
              <dt>
                <Key>Ctrl+Alt+{index + 2}</Key>
              </dt>
              <dd className="truncate text-muted-foreground">Jump to {device.name}</dd>
            </div>
          ))}
          <dt>
            <Key>Ctrl+Alt+Home</Key>
          </dt>
          <dd className="text-muted-foreground">Come back to this PC, whatever is happening</dd>
        </dl>
      </div>
      <SwitchRow
        label="Media shortcuts"
        description="These work from this PC's keyboard, wherever the cursor is, on whatever is playing on any of your PCs. If several are, it's the one that started last."
        checked={state.rules.mediaShortcuts.enabled}
        onChange={(enabled) =>
          update({ mediaShortcuts: { ...state.rules.mediaShortcuts, enabled } })
        }
      />
      <MediaShortcutRows shortcuts={state.rules.mediaShortcuts} update={update} />
    </>
  );
}

const MEDIA_SHORTCUT_ROWS: { field: keyof Omit<MediaShortcuts, "enabled">; label: string }[] = [
  { field: "playPause", label: "Play or pause" },
  { field: "back", label: "Back" },
  { field: "forward", label: "Forward" },
];

function MediaShortcutRows({
  shortcuts,
  update,
}: {
  shortcuts: MediaShortcuts;
  update: (change: Partial<Rules>) => void;
}) {
  const others = (field: string) =>
    MEDIA_SHORTCUT_ROWS.filter((row) => row.field !== field).map((row) => shortcuts[row.field]);
  const isUsual = MEDIA_SHORTCUT_ROWS.every(
    ({ field }) => describeShortcut(shortcuts[field]) === describeShortcut(USUAL_MEDIA_SHORTCUTS[field]),
  );
  return (
    <div className={cn("flex flex-col gap-2", !shortcuts.enabled && "opacity-50")}>
      {MEDIA_SHORTCUT_ROWS.map(({ field, label }) => (
        <ShortcutRow
          key={field}
          label={label}
          shortcut={shortcuts[field]}
          others={others(field)}
          disabled={!shortcuts.enabled}
          onChange={(shortcut) =>
            update({ mediaShortcuts: { ...shortcuts, [field]: shortcut } })
          }
        />
      ))}
      {!isUsual && (
        <div>
          <Button
            size="xs"
            variant="ghost"
            disabled={!shortcuts.enabled}
            onClick={() =>
              update({ mediaShortcuts: { enabled: shortcuts.enabled, ...USUAL_MEDIA_SHORTCUTS } })
            }
          >
            Put back the usual ones
          </Button>
        </div>
      )}
    </div>
  );
}

/** Shows a shortcut. Pressing the button waits for the next keys pressed, and they're the new one. */
function ShortcutRow({
  label,
  shortcut,
  others,
  disabled,
  onChange,
}: {
  label: string;
  shortcut: Shortcut;
  others: Shortcut[];
  disabled: boolean;
  onChange: (shortcut: Shortcut) => void;
}) {
  const [recording, setRecording] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const labels = useKeyLabels();

  // In the dependencies, these would stop and start it on every state from the agent.
  const othersKey = JSON.stringify(others);
  useEffect(() => {
    if (!recording) return;
    // The agent leaves shortcuts alone for now, or it'd act on them before this saw them.
    send({ type: "recordingShortcut", active: true });
    function onKey(event: KeyboardEvent) {
      event.preventDefault();
      event.stopPropagation();
      if (event.code === "Escape") {
        setProblem(null);
        setRecording(false);
        return;
      }
      if (isModifierKey(event.code)) return;
      const pressed = shortcutFromEvent(event);
      const wrong = event.getModifierState("AltGraph")
        ? "AltGr is for typing characters. Use the left Alt key."
        : shortcutProblem(pressed, JSON.parse(othersKey) as Shortcut[]);
      setProblem(wrong);
      if (!wrong) {
        onChange(pressed);
        setRecording(false);
      }
    }
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      send({ type: "recordingShortcut", active: false });
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording, othersKey]);

  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center justify-between gap-3">
        <span className="text-xs">{label}</span>
        <div className="flex items-center gap-1">
          <Button
            size="xs"
            variant="outline"
            disabled={disabled}
            aria-pressed={recording}
            aria-label={`Change the shortcut for ${label}: now ${describeShortcut(shortcut, labels)}`}
            onClick={() => {
              setProblem(null);
              setRecording(!recording);
            }}
            className={cn("min-w-32 font-mono", recording && "border-signal text-signal")}
          >
            {recording ? "Press the keys…" : describeShortcut(shortcut, labels)}
          </Button>
          <Button
            size="icon-xs"
            variant="ghost"
            disabled={disabled || !shortcut.key}
            title="Turn this one off"
            aria-label={`Turn off the shortcut for ${label}`}
            onClick={() => {
              setRecording(false);
              setProblem(null);
              onChange(NO_SHORTCUT);
            }}
          >
            <X />
          </Button>
        </div>
      </div>
      {recording && !problem && (
        <p className="text-right text-xs text-muted-foreground">Esc to stop without changing it.</p>
      )}
      {problem && (
        <p role="alert" className="text-right text-xs text-amber-700 dark:text-amber-400">
          {problem}
        </p>
      )}
    </div>
  );
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-6">
      <span className="shrink-0 text-sm font-medium">{label}</span>
      <span className="min-w-0 text-right text-xs break-words text-muted-foreground select-text">
        {children}
      </span>
    </div>
  );
}

function About({ state }: { state: UiState }) {
  const { me } = state;
  return (
    <>
      <Fact label="Version">{me.version}</Fact>
      {me.model && <Fact label="This computer">{me.model}</Fact>}
      <Fact label="Addresses">
        <span className="tabular-nums">{me.addresses.join(", ") || "Not on a network"}</span>
      </Fact>
      <Fact label="Port">
        <span className="tabular-nums">{me.port}</span>
      </Fact>
      <Fact label="This PC's ID">
        <span className="font-mono">{me.fingerprint.slice(0, 32)}…</span>
      </Fact>
      <Row
        label="Log"
        description="Records what happens with connections, the clipboard and files, never what's in them."
      >
        <Button size="sm" variant="outline" onClick={openLogFolder}>
          <FolderOpen aria-hidden />
          Open the folder
        </Button>
      </Row>
      <Fact label="License">MIT or Apache-2.0</Fact>
    </>
  );
}
