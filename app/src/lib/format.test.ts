import { describe, expect, it } from "vitest";

import { formatBytes, transferProgress } from "./format";

describe("formatBytes", () => {
  it("counts small sizes in bytes", () => {
    expect(formatBytes(0)).toBe("0 bytes");
    expect(formatBytes(1)).toBe("1 byte");
    expect(formatBytes(999)).toBe("999 bytes");
  });

  it("moves up a unit as sizes grow", () => {
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(5 * 1024 * 1024)).toBe("5.0 MB");
    expect(formatBytes(4_500_000_000)).toBe("4.2 GB");
    expect(formatBytes(300 * 1024 * 1024)).toBe("300 MB");
  });

  it("stops at the largest unit", () => {
    expect(formatBytes(5000 * 1024 ** 4)).toBe("5000 TB");
  });
});

describe("transferProgress", () => {
  it("rounds to a whole percentage", () => {
    expect(transferProgress(1, 3)).toBe(33);
    expect(transferProgress(10, 10)).toBe(100);
  });

  it("never passes 100 or divides by nothing", () => {
    expect(transferProgress(20, 10)).toBe(100);
    expect(transferProgress(0, 0)).toBe(0);
  });
});
