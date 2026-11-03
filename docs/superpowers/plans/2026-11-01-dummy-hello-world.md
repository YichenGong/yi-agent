# Dummy Hello World Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 产出一个打印 `Hello World` 的脚本，用来验证看板实现会话能正常跑完并留下可验证的产物。

**Architecture:** 单个 POSIX shell 脚本，零依赖，不接入任何构建或 CI 链路。作用是流水线烟囱测试，不承载业务逻辑。

**Tech Stack:** POSIX `sh`。

---

### Task 1: 新增脚本

**Files:**
- Create: `scripts/dummy-hello-world.sh`

- [ ] 新建 `scripts/dummy-hello-world.sh`，内容为一行 shebang `#!/usr/bin/env sh` 加一行 `echo "Hello World"`。
- [ ] 执行 `chmod +x scripts/dummy-hello-world.sh`。

### Task 2: 验证输出

**Files:**
- Test: `scripts/dummy-hello-world.sh`

- [ ] 执行 `sh scripts/dummy-hello-world.sh`，确认 stdout 为 `Hello World`。
- [ ] 执行 `test "$(sh scripts/dummy-hello-world.sh)" = "Hello World"`，确认退出码为 0。

### Task 3: 交付

**Files:**
- 无新增文件

- [ ] 确认 `git status` 只包含本次新增的脚本与这两份文档。
- [ ] 提交改动到本卡分支。
