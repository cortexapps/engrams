export interface ParsedHandle {
  provider: string;
  label: string;
  href?: string;
}

/** A forge's handle for a pull request: `<provider>:<repository>#<number>`. */
const PULL_REQUEST_HANDLE = /^([a-z]+):([^#:]+)#(\d+)$/;
const SLACK_HANDLE = /^slack:([^:]+)(?::(.+))?$/;

export function parseHandle(handle: string): ParsedHandle {
  const pullRequest = PULL_REQUEST_HANDLE.exec(handle);
  if (pullRequest) {
    const [, provider, repository, number] = pullRequest;
    return {
      provider: provider!,
      label: `${repository}#${number}`,
      // The handle has no host. Only GitHub's is known without one.
      ...(provider === "github" ? { href: `https://github.com/${repository}/pull/${number}` } : {}),
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
