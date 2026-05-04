import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Navigate, Route, Routes } from 'react-router-dom';
import { UserChip } from './components/UserChip';
import { Wordmark } from './components/Wordmark';
import { Overview } from './pages/Overview';
import { SessionDetail } from './pages/SessionDetail';
import { Settings } from './pages/Settings';
import { HarnessesPanel } from './components/settings/HarnessesPanel';
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
        {/* Both the wordmark (top-left, links to /) and the user
            chip (top-right) are rendered above the routed pages so
            they survive route transitions and stay anchored to the
            viewport corners. */}
        <Wordmark />
        <UserChip />
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
          <Route path="/settings" element={<Settings />}>
            <Route index element={<Navigate to="registries" replace />} />
            <Route path="registries" element={<RegistriesPanel />} />
            <Route path="harnesses" element={<HarnessesPanel />} />
            <Route path="profile" element={<ProfilePanel />} />
          </Route>
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
