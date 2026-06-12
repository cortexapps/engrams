/**
 * Orchestrator runtime configuration.
 *
 * All values are read from environment variables at startup. `loadConfig`
 * throws with a clear message listing every missing required variable so
 * the operator knows exactly what to set before the process can boot.
 */

export interface Config {
  /** ORCHESTRATOR_PORT — default 8787 */
  port: number;
  /** ORCHESTRATOR_DATABASE_URL — optional now; Task 15 makes it required */
  databaseUrl: string | undefined;
  /** CONTROL_PLANE_GRPC_URL — default http://127.0.0.1:50061 */
  controlPlaneGrpcUrl: string;
  /** CONTROL_PLANE_BEARER — required: the coordinator app-gRPC bearer token */
  controlPlaneBearer: string;
  /** TRUSTED_ORIGINS — comma-separated list; dev default: http://localhost:5173 */
  trustedOrigins: string[];
}

export function loadConfig(env: Record<string, string | undefined> = process.env): Config {
  const missing: string[] = [];

  function require(key: string): string {
    const v = env[key];
    if (!v) missing.push(key);
    return v ?? "";
  }

  function optional(key: string, fallback: string): string {
    return env[key] || fallback;
  }

  const portStr = optional("ORCHESTRATOR_PORT", "8787");
  const port = parseInt(portStr, 10);
  const controlPlaneBearer = require("CONTROL_PLANE_BEARER");
  const controlPlaneGrpcUrl = optional("CONTROL_PLANE_GRPC_URL", "http://127.0.0.1:50061");
  const databaseUrl = env["ORCHESTRATOR_DATABASE_URL"];
  const trustedOriginsRaw = optional("TRUSTED_ORIGINS", "http://localhost:5173");
  const trustedOrigins = trustedOriginsRaw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);

  if (missing.length > 0) {
    throw new Error(
      `Orchestrator: missing required environment variable(s): ${missing.join(", ")}`,
    );
  }

  if (isNaN(port) || port <= 0 || port > 65535) {
    throw new Error(
      `Orchestrator: ORCHESTRATOR_PORT="${portStr}" is not a valid port number`,
    );
  }

  return {
    port,
    databaseUrl,
    controlPlaneGrpcUrl,
    controlPlaneBearer,
    trustedOrigins,
  };
}

export const config: Config = loadConfig();
