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
