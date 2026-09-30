import { lazy, Suspense } from "react";
import Island from "./components/Island";
import { StoreProvider } from "./state/store";

// Loaded only in the workplace window, so the island never parses xterm.
const Workplace = lazy(() => import("./components/workplace/Workplace"));

export default function App({ windowLabel }: { windowLabel: string }) {
  return (
    <StoreProvider>
      {windowLabel === "workplace" ? (
        <Suspense fallback={null}>
          <Workplace />
        </Suspense>
      ) : (
        <Island />
      )}
    </StoreProvider>
  );
}
