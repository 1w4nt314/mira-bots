import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import "./styles.css";

// Routing is by window label ("island" | "workplace"); styles.css scopes rules on this attribute.
const label = getCurrentWindow().label;
document.documentElement.dataset.window = label;

createRoot(document.getElementById("root") as HTMLElement).render(
  <StrictMode>
    <App windowLabel={label} />
  </StrictMode>,
);
