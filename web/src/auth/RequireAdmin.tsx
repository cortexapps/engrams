// ADR 0031 route guard for admin-only Settings sub-routes. UX-only — the
// coordinator's require_admin layer is the real gate; this just keeps a member
// who deep-links to an admin route from rendering an empty/erroring panel.

import { Navigate } from 'react-router-dom';
import { type ReactNode } from 'react';
import { useIsAdmin } from './AuthProvider';

export function RequireAdmin({ children }: { children: ReactNode }) {
  if (!useIsAdmin()) {
    return <Navigate to="/settings/profile" replace />;
  }
  return <>{children}</>;
}
