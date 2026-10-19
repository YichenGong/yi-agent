import { Component, type ErrorInfo, type ReactNode } from "react";

/**
 * 最小的错误边界：把子树的渲染期异常**就地**收敛成 `fallback`，不让它冒泡到
 * 根组件而卸载整棵树。
 *
 * 为什么需要它：`PairingScreen` 的扫码覆盖层里跑着相机取流（`QrScanner` →
 * `startCameraScan`）。相机在 iOS 上失败的方式很杂（权限拒绝、属性不存在、元数据
 * 未就绪），任何一条如果变成渲染期异常，React 19 会把**整个 App**卸载——手机
 * `body` 底色是深色，用户看到的就是一屏黑、且无路可走。取流本身已经改成不抛
 * （见 `lib/qrScanner.ts`）；这一层是第二道保险：即便还有没预料到的异常，影响面
 * 也只落在被包住的那一小块上。
 *
 * `onError` 让宿主能顺手做收尾（例如把我们拉回可用的表单）。
 */
export interface ErrorBoundaryProps {
  children: ReactNode;
  /** 捕获到异常时回调一次，供宿主收尾（收起覆盖层、显示文案等）。 */
  onError?: (error: unknown) => void;
  /** 出错时渲染的内容；缺省什么都不渲染。 */
  fallback?: ReactNode;
}

interface ErrorBoundaryState {
  hasError: boolean;
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { hasError: false };

  static getDerivedStateFromError(): ErrorBoundaryState {
    return { hasError: true };
  }

  componentDidCatch(error: unknown, _info: ErrorInfo): void {
    this.props.onError?.(error);
  }

  render(): ReactNode {
    if (this.state.hasError) return this.props.fallback ?? null;
    return this.props.children;
  }
}