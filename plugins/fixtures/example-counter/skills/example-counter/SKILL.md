---
name: example-counter
description: 验证插件托管 skill 的最小样例：读计数、加一。用于确认 skill 来源绑定到已验证的发布包，而不是用户全局目录。
---

# example-counter

这是插件 `example-counter` 自带的 skill，用于验证两件事：

1. 托管 skill **绑定到来源**：同名自定义 skill 不能被当成发布包里的这一份。
2. skill 的发现不依赖用户全局目录的复制。

## 用法

计数保存在插件自己的受管数据目录里。读：

```bash
yi-agent plugins exec --plugin example-counter --project-handle ph1_... \
  --binding-file session-plugin-binding.json -- status
```

加一（写；需要 `Write` 授权）：

```bash
yi-agent plugins exec --plugin example-counter --project-handle ph1_... \
  --binding-file session-plugin-binding.json -- increment --by 1
```
