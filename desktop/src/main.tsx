import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { syncCacheScope } from "./lib/cacheScope";
import "./index.css";

// 必须在任何组件渲染之前跑一次：它是模块初始化期副作用，不是渲染期副作用。
// 放进 App.tsx 的 render body（如 initialListCache 那行旁边）会导致每次重渲染
// 都比对一遍并可能重写 localStorage，StrictMode 的双次渲染会放大这个问题。
syncCacheScope();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
