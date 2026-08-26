// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { defineLoader } from "vitepress";

// Build-time loader, matching `results.data.ts`: `watch` hot-reloads the page
// when a new run is dropped in, and pins the dependency so the production build
// cannot serve a stale table.
const DATA = fileURLToPath(
  new URL("./streaming-vs-tree.json", import.meta.url),
);

interface Row {
  case: string;
  // Criterion's slope estimate where it has one, mean otherwise. Never mixed
  // within a pair.
  stat: "slope" | "mean";
  candidate_ns: number;
  baseline_ns: number;
  speedup: number;
  // "faster" and "slower" mean the confidence intervals do not overlap. "no"
  // means they do, whatever the speedup column says.
  significant: "faster" | "slower" | "no";
}

interface Raw {
  quality: "ok" | "degraded";
  busy: number;
  rows: Row[];
}

export interface Data {
  rows: Array<Row & { streaming_us: string; tree_us: string; ratio: string }>;
  /** True only when medium AND large are significantly faster — the #58 rule. */
  verdict: boolean;
  quality: "ok" | "degraded";
  busy: string;
}

declare const data: Data;
export { data };

export default defineLoader({
  watch: [DATA],
  load(): Data {
    let raw: Raw;
    try {
      raw = JSON.parse(readFileSync(DATA, "utf-8")) as Raw;
    } catch {
      // No run recorded yet. An empty table is honest; a fabricated one is not.
      return { rows: [], verdict: false, quality: "degraded", busy: "—" };
    }
    const rows = raw.rows ?? [];

    const decisive = (name: string) =>
      rows.some((r) => r.case.includes(name) && r.significant === "faster");

    return {
      rows: rows.map((r) => ({
        ...r,
        streaming_us: (r.candidate_ns / 1000).toFixed(2),
        tree_us: (r.baseline_ns / 1000).toFixed(2),
        ratio: `${r.speedup.toFixed(2)}x`,
      })),
      // #58's rule: medium AND large, on non-overlapping intervals.
      verdict: decisive("medium") && decisive("large"),
      quality: raw.quality,
      busy: `${(raw.busy * 100).toFixed(0)}%`,
    };
  },
});
