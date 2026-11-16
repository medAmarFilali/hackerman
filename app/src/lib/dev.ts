import type {
  CacheKind,
  CleanItem,
  DiskReport,
  DockerClean,
  DockerReport,
  FolderKind,
  ProgramKind,
} from "./types";

/** In gigabytes as Windows counts them, 1024 megabytes each. */
export function formatSize(bytes: number): string {
  const gigabytes = bytes / 1024 ** 3;
  if (gigabytes >= 1) return `${gigabytes.toFixed(1)} GB`;
  return `${Math.round(bytes / 1024 ** 2)} MB`;
}

export const PROGRAM_KINDS: Record<ProgramKind, string> = {
  nextJs: "Next.js",
  vite: "Vite",
  webDevServer: "Dev server",
  node: "Node.js",
  dotNet: ".NET",
  rust: "Rust",
  docker: "Docker",
  buildHelper: "Build helper",
  editorHelper: "Editor helper",
  packageManager: "Package manager",
  other: "Program",
};

export const FOLDER_KINDS: Record<FolderKind, string> = {
  nodeModules: "node_modules",
  rustTarget: "Rust target",
  dotNetBuild: ".NET bin/obj",
  nextBuild: "Next.js .next",
};

/** `name` is what the cache is; `label` is what cleaning it deletes. */
export const CACHES: Record<CacheKind, { name: string; label: string; detail: string; restore: string }> = {
  npm: {
    name: "npm cache",
    label: "npm cache",
    detail: "Packages npm downloaded before.",
    restore: "Downloaded again by the next npm install.",
  },
  pnpmStore: {
    name: "pnpm store",
    label: "pnpm store, unused part",
    detail: "Packages no project uses any more. Ones in use stay.",
    restore: "Downloaded again if a project needs one.",
  },
  nuGetPackages: {
    name: "NuGet packages",
    label: "NuGet packages",
    detail: "Every package .NET projects use, kept once for all of them.",
    restore: "Downloaded again by the next dotnet restore or build.",
  },
  nuGetHttp: {
    name: "NuGet downloads",
    label: "NuGet downloads",
    detail: "Copies of what NuGet downloaded.",
    restore: "Downloaded again when needed.",
  },
  cargoRegistry: {
    name: "Cargo downloads",
    label: "Cargo downloads",
    detail: "Rust crates downloaded and unpacked. The list of what's available stays.",
    restore: "Downloaded again by the next cargo build.",
  },
};

const DOCKER: Record<DockerClean, { label: string; detail: string; restore: string }> = {
  buildCache: {
    label: "Docker build cache",
    detail: "Saved steps of earlier image builds.",
    restore: "The next build takes longer.",
  },
  danglingImages: {
    label: "Docker images with no name",
    detail: "Old versions left behind when an image was built again.",
    restore: "Nothing needs them.",
  },
  unusedImages: {
    label: "Docker images no container uses",
    detail: "Including ones you built yourself and never pushed anywhere.",
    restore: "docker pull, or docker build for your own.",
  },
  stoppedContainers: {
    label: "Stopped Docker containers",
    detail: "Anything saved inside them, rather than in a volume, goes with them.",
    restore: "Can't be. Run the image again for a new, empty one.",
  },
  unusedVolumes: {
    label: "Docker volumes no container uses",
    detail: "Volumes hold databases and uploaded files, which may exist nowhere else.",
    restore: "Can't be. What was in them is gone.",
  },
};

const RESTORE: Record<FolderKind, string> = {
  nodeModules: "npm install, pnpm install or yarn",
  rustTarget: "cargo build",
  dotNetBuild: "dotnet build",
  nextBuild: "next dev or next build",
};

export type Risk = "safe" | "slower" | "canLoseData";

export const RISKS: Record<Risk, { title: string; detail: string }> = {
  safe: {
    title: "Safe",
    detail: "Comes back by itself when it's needed. The next install or build downloads it again.",
  },
  slower: {
    title: "Slower next build",
    detail: "Comes back by building or installing again, which takes time and the internet.",
  },
  canLoseData: {
    title: "Can lose data",
    detail: "What's in these may exist nowhere else. Only choose them if you're sure.",
  },
};

/** How much harm cleaning something up can do. The same as on the PC doing it. */
export function riskOf(item: CleanItem): Risk {
  if ("cache" in item) return "safe";
  if ("folder" in item) return "slower";
  switch (item.docker.kind) {
    case "buildCache":
    case "danglingImages":
      return "safe";
    case "unusedImages":
      return "slower";
    default:
      return "canLoseData";
  }
}

/** Something that can be cleaned up, as the cleanup dialog lists it. */
export interface Choice {
  key: string;
  item: CleanItem;
  label: string;
  detail: string;
  /** What gets it back. */
  restore: string;
  /** About how much it frees. Unknown for some Docker kinds. */
  size: number | null;
  risk: Risk;
  /** Why it can't be chosen right now, if it can't. */
  blocked: string | null;
  lastUsed: number | null;
  /** Frees less than its size, as pnpm's folders share files with its store. */
  sharedFiles: boolean;
}

export function keyOf(item: CleanItem): string {
  if ("folder" in item) return `folder:${item.folder.path}`;
  if ("cache" in item) return `cache:${item.cache.kind}`;
  return `docker:${item.docker.kind}`;
}

/** Everything the PC said can be cleaned up, largest first within each risk. */
export function cleanupChoices(disk: DiskReport | null, docker: DockerReport | null): Choice[] {
  const choices: Choice[] = [];
  for (const cache of disk?.caches ?? []) {
    const about = CACHES[cache.kind];
    const item: CleanItem = { cache: { kind: cache.kind } };
    choices.push({
      key: keyOf(item),
      item,
      label: about.label,
      detail: `${about.detail} ${cache.path}`,
      restore: about.restore,
      size: cache.kind === "pnpmStore" ? null : cache.size,
      risk: riskOf(item),
      blocked: null,
      lastUsed: null,
      sharedFiles: false,
    });
  }
  for (const folder of disk?.folders ?? []) {
    const item: CleanItem = { folder: { path: folder.path } };
    choices.push({
      key: keyOf(item),
      item,
      label: `${folder.project}: ${FOLDER_KINDS[folder.kind]}`,
      detail: folder.path,
      restore: RESTORE[folder.kind],
      size: folder.size,
      risk: riskOf(item),
      blocked: folder.inUse ? "A program that's running is using this project." : null,
      lastUsed: folder.lastUsed,
      sharedFiles: folder.sharedFiles,
    });
  }
  if (docker?.running) {
    const kinds: DockerClean[] = [
      "buildCache",
      "danglingImages",
      "unusedImages",
      "stoppedContainers",
      "unusedVolumes",
    ];
    for (const kind of kinds) {
      const about = DOCKER[kind];
      const item: CleanItem = { docker: { kind } };
      const usage = docker.usage.find((found) => found.kind === kind);
      choices.push({
        key: keyOf(item),
        item,
        label: about.label,
        detail: about.detail,
        restore: about.restore,
        // Docker only says what all unused images come to, which those with no name are part of.
        size: kind === "danglingImages" ? null : (usage?.reclaimable ?? null),
        risk: riskOf(item),
        blocked: null,
        lastUsed: null,
        sharedFiles: false,
      });
    }
  }
  const order: Risk[] = ["safe", "slower", "canLoseData"];
  return choices.sort(
    (a, b) => order.indexOf(a.risk) - order.indexOf(b.risk) || (b.size ?? 0) - (a.size ?? 0),
  );
}

/** Whether a project was changed too recently to touch, by the choice of how recent. */
export function recentlyUsed(choice: Choice, keepDays: number, now: number): boolean {
  if (keepDays <= 0 || choice.lastUsed === null) return false;
  return now - choice.lastUsed < keepDays * 24 * 60 * 60 * 1000;
}

/** What choosing these comes to. */
export function summary(chosen: Choice[]): { size: number; unknown: number; risky: boolean } {
  return {
    size: chosen.reduce((total, choice) => total + (choice.size ?? 0), 0),
    unknown: chosen.filter((choice) => choice.size === null).length,
    risky: chosen.some((choice) => choice.risk === "canLoseData"),
  };
}

/** What has to be typed to delete anything that can lose data. */
export const CONFIRM_WORD = "DELETE";
