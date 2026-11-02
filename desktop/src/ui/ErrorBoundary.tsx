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
  /**
   * 该值变化时清掉 `hasError`，让子树在原位置恢复渲染。
   *
   * 为什么不用 React 的 `key`：`key` 变化会**卸载并重挂**整棵子树。转录区
   * 的 boundary 若用 `key={currentId}`，每次切会话都会重挂 `ChatView`，把
   * 滚动位置与展开的 reasoning 块全部丢掉——那正是本卡要修的场景。
   * `resetKey` 只重置错误位，子树原地保留。
   *
   * 不传时行为与历史版本逐字节一致（配对页那处调用就不传）。
   */
  resetKey?: unknown;
}

interface ErrorBoundaryState {
  hasError: boolean;
  /** 上一次生效的 `resetKey`，用来判断它是否真的变了。 */
  seenKey: unknown;
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { hasError: false, seenKey: this.props.resetKey };

  static getDerivedStateFromError(): Partial<ErrorBoundaryState> {
    return { hasError: true };
  }

  static getDerivedStateFromProps(
    props: ErrorBoundaryProps,
    state: ErrorBoundaryState,
  ): Partial<ErrorBoundaryState> | null {
    if (props.resetKey !== state.seenKey) {
      return { hasError: false, seenKey: props.resetKey };
    }
    // 必须返回 null（而不是 `{}`）：返回对象会触发一次多余的重渲染。
    return null;
  }

  componentDidCatch(error: unknown, _info: ErrorInfo): void {
    this.props.onError?.(error);
  }

  render(): ReactNode {
    if (this.state.hasError) return this.props.fallback ?? null;
    return this.props.children;
  }
}