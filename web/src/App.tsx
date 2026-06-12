import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { AuthProvider, useOptionalAuth } from "./auth/AuthProvider";
import { router } from "./router";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 0,
      refetchOnWindowFocus: false,
    },
  },
});

// ADR 0039 Task 22: AuthProvider recomposed on better-auth.
// AuthProvider resolves the principal (authClient.useSession + GET /api/v1/me/claude-token)
// and renders children once the session query has settled — loading → BootScreen;
// error → AuthErrorScreen; resolved-null OR resolved-session → render here.
//
// The auth gate (redirect signed-out requests → /login) lives in the router's
// appLayoutRoute.beforeLoad (router.tsx). This means /login is always reachable
// signed-out without a reload loop.
function InnerApp() {
  const auth = useOptionalAuth(); // null when signed out; AuthState when signed in
  return <RouterProvider router={router} context={{ auth }} />;
}

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <InnerApp />
      </AuthProvider>
    </QueryClientProvider>
  );
}
