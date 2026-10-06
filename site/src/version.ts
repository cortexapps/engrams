// The release these docs describe, read at build time from the control-plane
// chart: its appVersion is the version that `scripts/release-version.sh` sets
// and that a release tags. The build runs with site/ as the working directory.
import { readFileSync } from "node:fs";
import { join } from "node:path";

const chart = readFileSync(join(process.cwd(), "..", "deploy", "helm", "engram", "Chart.yaml"), "utf8");
const match = /^appVersion: "(\d+)\.(\d+)\.(\d+)"$/m.exec(chart);
if (!match) throw new Error("deploy/helm/engram/Chart.yaml has no appVersion of the form major.minor.patch");

/** The full version, for example `0.10.0`. */
export const VERSION = `${match[1]}.${match[2]}.${match[3]}`;
/** The label the docs show, for example `v0.10`. */
export const VERSION_LABEL = `v${match[1]}.${match[2]}`;
