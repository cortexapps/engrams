import { createAuthClient } from "better-auth/react";
import { adminClient } from "better-auth/client/plugins";

// Same-origin: the vite proxy (dev) / fronting LB (prod) routes /api/auth
// to the orchestrator. Don't set a cross-origin baseURL — cookie scoping
// requires same-origin delivery so the HttpOnly session cookie rides every
// subsequent same-origin request automatically.
export const authClient = createAuthClient({ plugins: [adminClient()] });
