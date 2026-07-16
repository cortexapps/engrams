const BROWSER_CLI = new Set(["agent-browser", "playwright-cli"]);
const SHELL_BROWSER_COMMAND =
  /(?:^|[;&|]\s*|\s)(?:[^\s]*\/)?(?:agent-browser|playwright-cli)\s+(?:open|goto|snapshot|read|click|fill|type|press|select|check|screenshot|record|trace|wait|tab|eval|run-code|close|attach)\b/;

/** True when an exec command will start or drive the shared in-guest browser. */
export function isSharedBrowserCommand(command: readonly string[]): boolean {
  const executable = command[0]?.split("/").pop();
  if (executable && BROWSER_CLI.has(executable)) return true;
  return SHELL_BROWSER_COMMAND.test(command.join(" "));
}
