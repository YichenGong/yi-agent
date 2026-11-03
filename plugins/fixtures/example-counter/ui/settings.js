// example-counter 的设置视图。
//
// 存在的意义是证明"第二个设置入口"由清单贡献，而不是宿主里多一行注册代码。
// 它同样只走 SDK。

import { connectOrNull } from "../../../../sdk/dist/index.js";

const detail = document.getElementById("detail");

async function bootstrap() {
  const host = await connectOrNull();
  if (!host) {
    if (detail) detail.textContent = "未在宿主中运行";
    return;
  }
  try {
    const description = await host.request("counters/describe", {});
    if (detail) {
      detail.textContent = `数据格式 v${description.dataFormat}，存储于受管数据目录。`;
    }
  } catch (error) {
    if (detail) detail.textContent = `读取失败：${error && error.detail ? error.detail : error}`;
  }
}

void bootstrap();
