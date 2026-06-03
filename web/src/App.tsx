import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { AuthProvider } from './auth/AuthProvider';
import { RequireAdmin } from './auth/RequireAdmin';
import { Layout } from './pages/Layout';
import { Sessions } from './pages/Sessions';
import { Fleet } from './pages/Fleet';
import { Storage } from './pages/Storage';
import { SessionDetail } from './pages/SessionDetail';
import { Settings } from './pages/Settings';
import { Members } from './pages/Members';
// ADR 0021 P1.5a retired the Harnesses settings panel + its route.
import { ImagesPanel } from './components/settings/ImagesPanel';
import { ProfilePanel } from './components/settings/ProfilePanel';
import { RegistriesPanel } from './components/settings/RegistriesPanel';
import { TokensPanel } from './components/settings/TokensPanel';

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 0,
      refetchOnWindowFocus: false,
    },
  },
});

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        {/* ADR 0031: AuthProvider gates the app behind a resolved principal
            (GET /me). In dev (synthetic admin) it resolves to a local admin
            with no login wall. */}
        <AuthProvider>
          {/* ADR 0029/0031: four-surface IA. Settings is now visible to all
              users (profile + tokens are user-facing). Admin-only config
              (Members / Images / Registries) is gated with RequireAdmin inside
              Settings rather than by hiding the whole tab. */}
          <Routes>
            <Route element={<Layout />}>
              <Route path="/" element={<Sessions />} />
              <Route path="/sessions/:id" element={<SessionDetail />} />
              <Route
                path="/fleet"
                element={
                  <RequireAdmin>
                    <Fleet />
                  </RequireAdmin>
                }
              />
              <Route
                path="/storage"
                element={
                  <RequireAdmin>
                    <Storage />
                  </RequireAdmin>
                }
              />
              <Route path="/settings" element={<Settings />}>
                <Route index element={<Navigate to="profile" replace />} />
                {/* User settings — every authenticated user. */}
                <Route path="profile" element={<ProfilePanel />} />
                <Route path="tokens" element={<TokensPanel />} />
                {/* Admin settings — global config. */}
                <Route
                  path="members"
                  element={
                    <RequireAdmin>
                      <Members />
                    </RequireAdmin>
                  }
                />
                <Route
                  path="images"
                  element={
                    <RequireAdmin>
                      <ImagesPanel />
                    </RequireAdmin>
                  }
                />
                <Route
                  path="registries"
                  element={
                    <RequireAdmin>
                      <RegistriesPanel />
                    </RequireAdmin>
                  }
                />
              </Route>
            </Route>
          </Routes>
        </AuthProvider>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
