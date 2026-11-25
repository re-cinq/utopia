import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { RouterProvider } from "@tanstack/react-router";
import { router } from "./router";
import { ToastHost } from "./toast";
import { DEFAULT_STALE_MS, applyQueryDefaults } from "./queryDefaults";
import { initTheme } from "./theme";
import "./styles.css";

// The theme is applied to <html> before rendering: the canvas reads the token for the first time and it must already be correct
initTheme();

// staleTime is tiered by key and the table lives in queryDefaults.ts; only the global default is set here (#518)
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: false,
      refetchOnWindowFocus: false,
      staleTime: DEFAULT_STALE_MS,
    },
  },
});
applyQueryDefaults(queryClient);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
      <ToastHost />
    </QueryClientProvider>
  </StrictMode>,
);
