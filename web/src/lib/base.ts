/**
 * Base URL for Engram API routes.
 *
 * Every coordinator and orchestrator HTTP route lives under `/api/v1`.
 * Exported here (single source of truth) so SSE, WS, and artifact URL
 * builders don't need to import from the now-deleted api.ts.
 *
 * Same-origin in dev (Vite proxy → :8787 orchestrator in all cases after
 * Task 28) and prod (nginx). The SPA owns the root path namespace so
 * deep-links like `/sessions/:id` never collide with `/api/v1/sessions/:id`.
 */
export const API_BASE = "/api/v1";
