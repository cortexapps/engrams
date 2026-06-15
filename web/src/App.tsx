import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { TransportProvider } from "@connectrpc/connect-query";
import { createConnectTransport } from "@connectrpc/connect-web";
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

// ADR 0051 Task 23: Connect transport targeting /rpc (Vite proxy → orchestrator :8787).
// TransportProvider makes it available to all useQuery/useMutation connect-query hooks
// without threading a transport prop everywhere.
const transport = createConnectTransport({ baseUrl: "/rpc" });

function InnerApp() {
  const auth = useOptionalAuth();
  return <RouterProvider router={router} context={{ auth }} />;
}

export function App() {
  return (
    <TransportProvider transport={transport}>
      <QueryClientProvider client={queryClient}>
        <AuthProvider>
          <InnerApp />
        </AuthProvider>
      </QueryClientProvider>
    </TransportProvider>
  );
}
