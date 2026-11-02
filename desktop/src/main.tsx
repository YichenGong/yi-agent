import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { syncCacheScope } from "./lib/cacheScope";
import { ErrorBoundary } from "./ui/ErrorBoundary";
import { AppCrashedScreen } from "./ui/AppCrashedScreen";
import "./index.css";

// 必须在任何组件渲染之前跑一次：它是模块初始化期副作用，不是渲染期副作用。
// 放进 App.tsx 的 render body（如 initialListCache 那行旁边）会导致每次重渲染
// 都比对一遍并可能重写 localStorage，StrictMode 的双次渲染会放大这个问题。
syncCacheScope();

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    {/*
      根级兜底：没有它，任何渲染期异常都会让 React 19 卸载整棵树——手机深色底上
      就是一屏黑，且没有任何恢复入口（设计 §3 R4 / PROBE-E）。它必须在
      `React.StrictMode` 之内、`<App />` 之外，才能接住 App 自身的渲染期异常。
    */}
    <ErrorBoundary
      onError={(e) => {
        // 只留证据，不改渲染：这类异常常常是预期内的失败（一次失败的 RPC）。
        console.error("app render failed", e);
      }}
      fallback={<AppCrashedScreen />}
    >
      <App />
    </ErrorBoundary>
  </React.StrictMode>,
);

// 同一类问题在异步路径上不会经过 React 的 boundary：留一条日志便于下次排查，
// 但**不**改渲染——接住它们也不会让界面变好，反而会制造误报。
window.addEventListener("unhandledrejection", (e) => {
  console.error("unhandled rejection", e.reason);
});
window.addEventListener("error", (e) => {
  console.error("window error", e.error ?? e.message);
});
