export interface ParsedHandle {
  provider: "slack" | "github" | string;
  label: string;
  href?: string;
}

const GITHUB_HANDLE = /^github:([^#]+)#(\d+)$/;
const SLACK_HANDLE = /^slack:([^:]+)(?::(.+))?$/;

export function parseHandle(handle: string): ParsedHandle {
  const github = GITHUB_HANDLE.exec(handle);
  if (github) {
    const repository = github[1]!;
    const number = github[2]!;
    return {
      provider: "github",
      label: `${repository}#${number}`,
      href: `https://github.com/${repository}/pull/${number}`,
    };
  }

  const slack = SLACK_HANDLE.exec(handle);
  if (slack) {
    return {
      provider: "slack",
      label: `#${slack[1]!}${slack[2] ? " · thread" : ""}`,
    };
  }

  return { provider: handle.split(":", 1)[0] || handle, label: handle };
}

/** A scope key made readable: `project-ENG_1` → `project ENG 1`. */
export function humanizeKey(key: string): string {
  return key.replace(/[-_]+/g, " ");
}

/** What an agent is called: the label it was started with, else the place it
 * owns (`owner/repo#123`, `#channel · thread`), else its key made readable. */
export function agentTitle(
  instance: { key: string; label?: string },
  handles: readonly string[],
): string {
  if (instance.label) return instance.label;
  const first = handles[0];
  if (first) return parseHandle(first).label;
  return humanizeKey(instance.key);
}
