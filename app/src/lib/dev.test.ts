import { describe, expect, it } from "vitest";

import { cleanupChoices, formatSize, recentlyUsed, riskOf, summary } from "./dev";
import type { DiskReport, DockerReport } from "./types";

const DAY = 24 * 60 * 60 * 1000;
const NOW = Date.UTC(2026, 10, 16);

const disk: DiskReport = {
  drives: [{ name: "C:", total: 1e12, free: 5e11 }],
  folders: [
    {
      path: "C:\\code\\site\\node_modules",
      kind: "nodeModules",
      project: "site",
      size: 900 * 1024 ** 2,
      lastUsed: NOW - 2 * DAY,
      inUse: false,
      sharedFiles: true,
    },
    {
      path: "C:\\code\\hackerman\\target",
      kind: "rustTarget",
      project: "hackerman",
      size: 90 * 1024 ** 3,
      lastUsed: NOW - 100 * DAY,
      inUse: true,
      sharedFiles: false,
    },
  ],
  caches: [{ kind: "npm", path: "C:\\Users\\one\\AppData\\Local\\npm-cache", size: 5 * 1024 ** 3 }],
  roots: ["C:\\code"],
  dockerImage: null,
};

const docker: DockerReport = {
  running: true,
  installed: true,
  containers: [],
  usage: [
    { kind: "buildCache", size: 3 * 1024 ** 3, reclaimable: 3 * 1024 ** 3 },
    { kind: "unusedVolumes", size: 1024 ** 3, reclaimable: 512 * 1024 ** 2 },
  ],
  diskImage: null,
};

describe("cleanup choices", () => {
  it("puts what's safe first and what can lose data last", () => {
    const risks = cleanupChoices(disk, docker).map((choice) => choice.risk);
    expect(risks).toEqual([...risks].sort((a, b) => ["safe", "slower", "canLoseData"].indexOf(a) - ["safe", "slower", "canLoseData"].indexOf(b)));
    expect(risks[0]).toBe("safe");
    expect(risks.at(-1)).toBe("canLoseData");
  });

  it("never offers what a running program is using", () => {
    const target = cleanupChoices(disk, null).find((choice) => choice.label.startsWith("hackerman"));
    expect(target?.blocked).toContain("running");
  });

  it("says what pnpm's folders really free", () => {
    const site = cleanupChoices(disk, null).find((choice) => choice.label.startsWith("site"));
    expect(site?.sharedFiles).toBe(true);
    expect(site?.restore).toContain("pnpm install");
  });

  it("only offers Docker's while Docker is running", () => {
    expect(cleanupChoices(disk, { ...docker, running: false }).some((c) => "docker" in c.item)).toBe(false);
    const volumes = cleanupChoices(null, docker).find((c) => c.label.includes("volumes"));
    expect(volumes?.risk).toBe("canLoseData");
    expect(volumes?.size).toBe(512 * 1024 ** 2);
  });
});

describe("recently used", () => {
  it("keeps projects changed lately, if asked to", () => {
    const [site, target] = cleanupChoices(disk, null).filter((choice) => "folder" in choice.item);
    const recent = [site, target].find((choice) => choice.label.startsWith("site"))!;
    const old = [site, target].find((choice) => choice.label.startsWith("hackerman"))!;
    expect(recentlyUsed(recent, 7, NOW)).toBe(true);
    expect(recentlyUsed(old, 30, NOW)).toBe(false);
    expect(recentlyUsed(recent, 0, NOW)).toBe(false);
  });
});

describe("summary", () => {
  it("adds up what's known and flags what can lose data", () => {
    const choices = cleanupChoices(disk, docker);
    const safe = choices.filter((choice) => choice.risk === "safe");
    expect(summary(safe).risky).toBe(false);
    expect(summary(choices).risky).toBe(true);
    expect(summary(safe).size).toBe(8 * 1024 ** 3);
  });
});

describe("sizes", () => {
  it("are in gigabytes as Windows counts them", () => {
    expect(formatSize(5 * 1024 ** 3)).toBe("5.0 GB");
    expect(formatSize(300 * 1024 ** 2)).toBe("300 MB");
  });

  it("risk matches the PC doing the cleaning", () => {
    expect(riskOf({ cache: { kind: "npm" } })).toBe("safe");
    expect(riskOf({ folder: { path: "x" } })).toBe("slower");
    expect(riskOf({ docker: { kind: "unusedImages" } })).toBe("slower");
    expect(riskOf({ docker: { kind: "stoppedContainers" } })).toBe("canLoseData");
  });
});
