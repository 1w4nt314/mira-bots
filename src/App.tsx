import Island from "./components/Island";
import { StoreProvider } from "./state/store";

export default function App() {
  return (
    <StoreProvider>
      <Island />
    </StoreProvider>
  );
}
