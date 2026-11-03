// example-counter 的主视图。
//
// 只通过 `@yi-agent/plugin-sdk` 与宿主对话：页面里没有任何"我是谁"的参数，
// 也没有 Tauri 的 invoke。宿主决定这个 frame 属于哪个插件、哪个项目、哪个
// 激活代次；本文件用到的全部能力就是声明过的两个业务方法。
//
// 刻意不用打包器：这个 fixture 要能在任何静态服务器上直接跑，这样"页面能不能
// 加载"与"构建链是否正常"是两件可以分开判断的事。

import { connectOrNull } from "../../../../sdk/dist/index.js";

const totalEl = document.getElementById("total");
const button = document.getElementById("increment");
const hint = document.getElementById("hint");

/** 视图状态机。每个状态都可见，不用一个布尔糊住。 */
function render(state, detail) {
  if (!totalEl || !button || !hint) return;
  switch (state) {
    case "connecting":
      totalEl.textContent = "正在连接宿主…";
      button.disabled = true;
      hint.textContent = "";
      return;
    case "no-host":
      // 被直接当静态页打开时也必须给出可读结果，不能永远停在"读取中"。
      totalEl.textContent = "未在宿主中运行";
      button.disabled = true;
      hint.textContent = "这个页面需要在 yi-agent 的插件视图里打开。";
      return;
    case "read-only":
      totalEl.textContent = `计数：${detail}`;
      button.disabled = true;
      hint.textContent = "当前项目只授予了只读能力，+1 已隐藏。";
      return;
    case "ready":
      totalEl.textContent = `计数：${detail}`;
      button.disabled = false;
      hint.textContent = "";
      return;
    case "error":
      totalEl.textContent = "读取失败";
      button.disabled = true;
      hint.textContent = String(detail);
      return;
    default:
      return;
  }
}

async function bootstrap() {
  render("connecting");
  const host = await connectOrNull();
  if (!host) {
    render("no-host");
    return;
  }

  const read = async () => {
    try {
      const { total } = await host.request("counters/read", {});
      render(button?.disabled === true ? "read-only" : "ready", total);
    } catch (error) {
      if (error && error.kind === "PermissionDenied") {
        render("read-only", 0);
        return;
      }
      render("error", error && error.detail ? error.detail : String(error));
    }
  };

  button?.addEventListener("click", async () => {
    button.disabled = true;
    try {
      const { total } = await host.request("counters/increment", { by: 1 });
      render("ready", total);
    } catch (error) {
      render("error", error && error.detail ? error.detail : String(error));
    }
  });

  await read();
}

void bootstrap();
