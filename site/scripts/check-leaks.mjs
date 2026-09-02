#!/usr/bin/env node
// The publication gate for site/: nothing internal and nothing that reads as
// machine-written may reach the public site. Runs as `pnpm lint`, inside
// `pnpm build`, and in the CI site lane. Exit 1 on any hit.
//
// A line may opt out with `leak-ok: <reason>` (an HTML comment in Markdown,
// a // comment in code). The reason is mandatory and every allowed line is
// printed so reviewers see it. There is no file-level or rule-level disable.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { extname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const SCAN = ["src", "public", "astro.config.mjs", "README.md"];
const TEXT = new Set([
  ".md", ".mdx", ".astro", ".ts", ".mjs", ".js", ".css", ".json", ".svg", ".txt", ".yml", ".yaml", ".html",
]);

// Words and frames that mark prose as machine-written. Case-insensitive.
const AI_TELL_WORDS = [
  "seamless", "seamlessly", "robust", "leverage", "leverages", "leveraging", "powerful",
  "cutting-edge", "state-of-the-art", "blazing", "blazingly", "effortless", "effortlessly",
  "elevate", "empower", "empowers", "unlock", "unlocks", "streamline", "streamlines", "delve",
  "dive in", "holistic", "comprehensive", "best-in-class", "world-class", "game-changing",
  "supercharge", "battle-tested", "production-ready",
];
const AI_TELL_FRAMES = [
  "In this guide", "Let's dive", "Let's get started", "This guide will show",
  "by running the following command", "Whether you're", "Whether you are",
  "It's important to note", "It is important to note", "In today's", "At its core", "the power of",
  "It's worth noting", "It is worth noting",
];
const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

const RULES = [
  { id: "adr", re: /\bADRs?\b/g, why: "decision records are internal; the public site never cites them" },
  { id: "adr-path", re: /\badr\/|\bdocs\/adr\b/g, why: "repo path to the decision records" },
  { id: "internal-repo", re: /engrams-internal/g, why: "the private companion repo" },
  { id: "dev-box", re: /\bengram-dev\b/g, why: "the internal dev machine" },
  { id: "claude-dir", re: /\.claude\//g, why: "personal skills and scripts directory" },
  { id: "hosted", re: /engrams\.cortex\.io/g, why: "the site does not mention a hosted deployment" },
  { id: "license", re: /\bApache\b/g, why: "the license is AGPL-3.0; only the harness SDK crates are Apache-2.0 (leak-ok)" },
  { id: "brand-case", re: /\bEngrams?\b/g, why: "the product name is lowercase engrams", prose: true },
  {
    id: "ai-tell",
    re: new RegExp(`\\b(?:${AI_TELL_WORDS.map(escape).join("|")})\\b`, "gi"),
    why: "reads as machine-written",
    prose: true,
  },
  {
    id: "ai-tell",
    re: new RegExp(`(?:${AI_TELL_FRAMES.map(escape).join("|")})`, "g"),
    why: "reads as machine-written",
    prose: true,
  },
];
const ALLOW = /leak-ok:\s*\S/;

function* walk(path) {
  const st = statSync(path);
  if (st.isDirectory()) {
    for (const name of readdirSync(path)) {
      if (name === "node_modules" || name === "dist" || name === ".astro") continue;
      yield* walk(join(path, name));
    }
  } else if (TEXT.has(extname(path))) {
    yield path;
  }
}

/** Strip fenced code and inline code so prose rules do not fire on identifiers. */
function proseView(lines) {
  const out = [];
  let fenced = false;
  for (const line of lines) {
    if (/^\s*(```|~~~)/.test(line)) {
      fenced = !fenced;
      out.push("");
      continue;
    }
    if (fenced) {
      out.push("");
      continue;
    }
    out.push(line.replace(/`[^`]*`/g, (m) => " ".repeat(m.length)));
  }
  return out;
}

let hits = 0;
let allowed = 0;
let files = 0;
const allowedLines = [];
const gha = Boolean(process.env.GITHUB_ACTIONS);

for (const start of SCAN) {
  let abs;
  try {
    abs = join(ROOT, start);
    statSync(abs);
  } catch {
    continue;
  }
  for (const file of walk(abs)) {
    files += 1;
    const rel = relative(ROOT, file);
    const lines = readFileSync(file, "utf8").split("\n");
    const prose = proseView(lines);
    lines.forEach((line, i) => {
      if (ALLOW.test(line)) {
        allowed += 1;
        allowedLines.push(`${rel}:${i + 1}: ${line.trim()}`);
        return;
      }
      for (const rule of RULES) {
        if (rule.skipFiles?.has(rel)) continue;
        const subject = rule.prose ? prose[i] : line;
        for (const m of subject.matchAll(rule.re)) {
          hits += 1;
          const col = m.index + 1;
          console.log(`${rel}:${i + 1}:${col}: [${rule.id}] "${m[0]}" — ${rule.why}`);
          if (gha) console.log(`::error file=site/${rel},line=${i + 1},col=${col}::[${rule.id}] ${m[0]} — ${rule.why}`);
        }
      }
    });
  }
}

if (allowedLines.length) {
  console.log("\nallowed (leak-ok):");
  for (const l of allowedLines) console.log(`  ${l}`);
}
console.log(`\nleak lint: ${files} files, ${hits} hits, ${allowed} allowed`);
process.exit(hits ? 1 : 0);
