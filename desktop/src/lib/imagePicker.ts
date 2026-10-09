/**
 * 远端（iOS）图片选择器：webview 自带的 `<input type="file">`。
 *
 * 放在独立的 lib 里而不是塞在 `App.tsx`：它是一段自管生命周期的 DOM 代码（造
 * input、挂监听、兜底取消、清理），有独立的状态机，和一个能被单测直接断言的
 * 泄漏面（`window` 上的 focus 监听）。`App` 只管调用与结果。
 */

/**
 * `window` focus 兜底监听的最长存活时间。
 *
 * `change`/`cancel` 才是正常的两条收尾路；`focus` 只服务老 webview（没有 `cancel`
 * 事件）的取消场景。若三条路都没来（picker 支起却没回任何事件），这条监听会一直
 * 挂在 `window` 上。定时器只负责**摘掉这层兜底监听**，不 resolve——见 `pickImageFiles`。
 */
export const PICKER_FOCUS_FALLBACK_MS = 30_000;

/**
 * 用 webview 自带的文件选择器选图片，返回用户选中的文件（取消为空数组）。
 *
 * 远端（iOS）的图片入口：这里没有 Tauri dialog（那是桌面端能力），相册由系统
 * 选择器交付，`accept="image/*"` 让系统只显示图片，`multiple` 允许多选。
 *
 * 选择器**当场造、用完即摘**：没有任何 UI 依赖它长期存在，挂一个隐藏 input 在
 * DOM 里只会多一份要维护的状态。摘除放在 `change`/`cancel` 之后——提前摘会让某些
 * 浏览器丢掉选择结果。input 必须在文档里（`display:none` 只是别占位），否则 iOS
 * 的 `click()` 不会打开系统选择器。
 *
 * 取消的兜底有两层：`cancel` 是较新的事件（Safari 16.4+），老 webview 上退而用
 * 「窗口重新获得焦点」判断对话框已关（把读数推迟一拍，让 `change` 先到）。
 */
export function pickImageFiles(): Promise<File[]> {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "image/*";
    input.multiple = true;
    input.style.display = "none";

    // 只 resolve 一次：change 与 focus 兜底可能都想收尾，先到者摘掉 input，后到者
    // 被「input 还在文档里吗」挡住（见下）。
    //
    // 摘掉 focus 兜底还有一个**定时**的出口（`fallbackTimer`）：三条事件路都没来时，
    // 这层兜底监听不该陪跑到会话结束。定时器只摘监听、**绝不 resolve**——晚到的
    // change/cancel 仍能正常收尾，故不会出现「慢选择器被误判成取消」。
    const done = (files: File[]) => {
      clearTimeout(fallbackTimer);
      input.remove();
      window.removeEventListener("focus", onFocus);
      resolve(files);
    };
    const onFocus = () => {
      // 推迟一拍：`change` 通常紧跟着 focus 到（甚至更早），先给它机会。
      setTimeout(() => {
        if (document.body.contains(input)) done(Array.from(input.files ?? []));
      }, 0);
    };
    // 定时器句柄要在 `done` 里能清掉，但它又是 `done` 闭包捕获的——先声明、后赋值。
    let fallbackTimer: ReturnType<typeof setTimeout>;
    fallbackTimer = setTimeout(() => {
      window.removeEventListener("focus", onFocus);
    }, PICKER_FOCUS_FALLBACK_MS);

    input.addEventListener("change", () => done(Array.from(input.files ?? [])));
    input.addEventListener("cancel", () => done([]));
    window.addEventListener("focus", onFocus);
    document.body.append(input);
    input.click();
  });
}
