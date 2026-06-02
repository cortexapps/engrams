import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { Layout } from './pages/Layout';
import { Sessions } from './pages/Sessions';
import { Fleet } from './pages/Fleet';
import { Storage } from './pages/Storage';
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
        {/* ADR 0029: the four-surface IA. Every surface renders inside
            the shared Layout (sticky nav spine + footer). Session
            detail drills in UNDER Sessions — the spine keeps the
            Sessions tab active and shows a `↳ <short id>` sub-crumb.
            Settings keeps its nested image/registry/profile children. */}
        <Routes>
          <Route element={<Layout />}>
            <Route path="/" element={<Sessions />} />
            <Route path="/sessions/:id" element={<SessionDetail />} />
            <Route path="/fleet" element={<Fleet />} />
            <Route path="/storage" element={<Storage />} />
            <Route path="/settings" element={<Settings />}>
              <Route index element={<Navigate to="images" replace />} />
              <Route path="images" element={<ImagesPanel />} />
              <Route path="registries" element={<RegistriesPanel />} />
              <Route path="profile" element={<ProfilePanel />} />
            </Route>
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
