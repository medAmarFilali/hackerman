import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import "./index.css";
import { startFileDrag } from "./lib/file-drag";
import { startTheme } from "./lib/theme";
import { connectToAgent } from "./store";

startTheme();

// This is an app window, not a web page: no Back and Reload menu.
window.addEventListener("contextmenu", (event) => {
  if (!(event.target instanceof HTMLInputElement)) {
    event.preventDefault();
  }
});

void connectToAgent();
startFileDrag();

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
