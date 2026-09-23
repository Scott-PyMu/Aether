import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./styles.css";

const container = document.getElementById("root");
if (container === null) {
  throw new Error("#root 容器缺失，无法挂载 Aether UI");
}

// 前端骨架（M3-01）：TanStack Query 客户端（会话/命令查询随 M3-02 接入）。
const queryClient = new QueryClient();

createRoot(container).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </React.StrictMode>,
);
