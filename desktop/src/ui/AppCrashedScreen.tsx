/**
 * 根级崩溃页：整个 App 的渲染期异常被 `main.tsx` 的 `ErrorBoundary` 接住后
 * 显示它。
 *
 * 硬约束：**不依赖任何 App 状态、不 import 任何业务组件**。它可能在 App
 * 初始化到一半时被挂上（那时 store / prefs / RPC 都不可信），任何外部依赖
 * 都可能让兜底本身再抛一次。样式只用 `index.css` 的语义色 token 类。
 *
 * `onReload` 可注入只是为了单测能断言到行为（jsdom 里 `location.reload` 不可 spy）；
 * 生产一律用默认值。
 */
export function AppCrashedScreen({ onReload }: { onReload?: () => void }) {
  return (
    <div className="flex h-screen w-screen flex-col items-center justify-center gap-4 bg-surface px-6 text-center text-fg">
      <div className="text-base font-medium">界面出了点问题</div>
      <p className="max-w-sm text-sm text-fg-muted">
        刚才的某个界面错误让整个页面停了下来。会话内容都在服务端，重新加载不会
        丢东西。
      </p>
      <button
        type="button"
        onClick={() => (onReload ?? (() => window.location.reload()))()}
        className="rounded-md border border-line-strong px-4 py-2 text-sm hover:bg-raised"
      >
        重新加载
      </button>
    </div>
  );
}
