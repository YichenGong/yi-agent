/**
 * 组长的 IM 式头像。**纯 CSS**：不引入图片资源（避免二进制资源与打包路径问题，
 * 见 spec §7.2）。用一个带首字母的圆点，配 `data-lead-avatar` 供测试与后续样式
 * 挂钩。
 *
 * 配色用 `bg-fg` / `text-surface`（前景与底色互换）而非强调色：本项目主题只有
 * surface/panel/raised/line/fg 这几档灰阶，没有 accent 系列；互换色在深浅两套
 * 主题下都自动保持高对比，正是 IM 头像的经典观感，也让它与普通会话行区分得开。
 */
export function LeadAvatar({ size = 20 }: { size?: number }) {
  return (
    <span
      data-lead-avatar=""
      aria-hidden="true"
      className="inline-flex shrink-0 items-center justify-center rounded-full bg-fg font-semibold text-surface"
      style={{ width: size, height: size, fontSize: Math.round(size * 0.5) }}
    >
      L
    </span>
  );
}
