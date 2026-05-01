import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter, Route, Routes } from 'react-router-dom';
import { Overview } from './pages/Overview';
import { SessionDetail } from './pages/SessionDetail';

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
        <Routes>
          <Route path="/" element={<Overview />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
