/**
 * /api/auth/* — better-auth handler mount (ADR 0039 §5 / Task 16).
 *
 * better-auth handles all its own sub-routes under this prefix:
 *   POST /api/auth/sign-up/email
 *   POST /api/auth/sign-in/email
 *   POST /api/auth/sign-out
 *   GET  /api/auth/get-session
 *   POST /api/auth/admin/set-role      (admin plugin)
 *   POST /api/auth/admin/ban-user      (admin plugin)
 *   GET  /api/auth/admin/list-users    (admin plugin)
 *   ... (other better-auth built-ins)
 *
 * We pass c.req.raw (the native Request) directly to auth.handler()
 * so better-auth receives the full request with all headers, body, and
 * URL untouched. The raw Response is returned directly to Hono.
 */

import { Hono } from "hono";
import { auth } from "../auth/better-auth.ts";

const authRoute = new Hono();

authRoute.on(["GET", "POST"], "/api/auth/*", (c) =>
  auth.handler(c.req.raw),
);

export default authRoute;
