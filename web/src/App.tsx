import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { UserChip } from './components/UserChip';
import { Overview } from './pages/Overview';
import { SessionDetail } from './pages/SessionDetail';
import { Settings } from './pages/Settings';
// ADR 0021 P1.5a retired the Harnesses settings panel + its route.
import { ImagesPanel } from './components/settings/ImagesPanel';
import { ProfilePanel } from './components/settings/ProfilePanel';
import { RegistriesPanel } from './components/settings/RegistriesPanel';

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
        {/* User chip is the only persistent corner element. The
            "back to overview" affordance lives in inner pages'
            H1 prefix ("engrams › settings"), not as separate
            chrome — see Settings.tsx. */}
        <UserChip />
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
          <Route path="/settings" element={<Settings />}>
            <Route index element={<Navigate to="images" replace />} />
            <Route path="images" element={<ImagesPanel />} />
            <Route path="registries" element={<RegistriesPanel />} />
            <Route path="profile" element={<ProfilePanel />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
