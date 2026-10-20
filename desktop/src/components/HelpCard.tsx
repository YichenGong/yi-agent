/**
 * `/help` 的输出卡片。
 *
 * 取代此前把 `renderHelp()` 的多行等宽文本塞进居中灰胶囊的做法（三行就撑破、
 * 对齐全靠空格）。外壳与 `ConfigCard` 同一套视觉语言：圆角卡片 + 小字分组标题
 * + 键值行，这样 `/help` 与 `/config` 一眼就是同一类「命令输出」。
 *
 * 三种形态：
 * - 无 target：完整目录，按 `HELP_GROUPS` 分组。
 * - target 命中：单条命令的详情（用法 / 说明）。
 * - target 未命中或宿主带 error：红字报错，**下面照常给完整目录**——「拼错了」
 *   的下一句往往是「那到底有哪些」，不该逼用户再敲一次 `/help`。
 */
import { HELP_GROUPS, SLASH_COMMANDS, type SlashCommandSpec } from "../lib/slash";

const EM_DASH = "—";

/** `/help /cost` 与 `/help cost` 等价；`/` 归一化后是空名。 */
function normalizeTarget(target?: string | null): string {
  return (target ?? "").trim().replace(/^\//, "");
}

function byName(name: string): SlashCommandSpec | undefined {
  return SLASH_COMMANDS.find((c) => c.name === name);
}

/**
 * 命令 + 可选用法，等宽。
 *
 * 宽度是量出来的：最长项 `/help [command]` 在 `text-sm` 等宽下约 126px，
 * `8.5rem`（136px）留出渲染余量又不会把说明列推得太远。`ConfigCard` 的标签列
 * 是 `w-24`——那里装的是中文短标签，这里要装带参数的等宽命令，所以不能照抄。
 */
function CommandLabel({ command }: { command: SlashCommandSpec }) {
  return (
    <span className="w-[8.5rem] shrink-0 whitespace-nowrap font-mono text-fg">
      /{command.name}
      {command.usage ? ` ${command.usage}` : ""}
    </span>
  );
}

/** 完整目录：每个分组一个小标题，下面是「命令 + 说明」行。 */
function Catalog() {
  return (
    <div className="flex flex-col gap-2">
      {HELP_GROUPS.map((group) => (
        <div key={group.title} className="flex flex-col gap-0.5">
          <div className="text-xs text-fg-subtle">{group.title}</div>
          {group.commands.map((name) => {
            // 分组表与目录由测试绑定；查不到就跳过而不是渲染半行空壳。
            const command = byName(name);
            if (!command) return null;
            return (
              <div key={command.name} className="flex gap-3">
                <CommandLabel command={command} />
                <span className="min-w-0 break-all text-fg-muted">
                  {command.description}
                </span>
              </div>
            );
          })}
        </div>
      ))}
    </div>
  );
}

/** 单条详情：用法 / 说明两行，标签列宽与 `ConfigCard` 一致（`w-24`）。 */
function Detail({ command }: { command: SlashCommandSpec }) {
  return (
    <div className="flex flex-col gap-0.5">
      <div className="flex gap-3">
        <span className="w-24 shrink-0 text-fg-muted">用法</span>
        <span className="min-w-0 break-all font-mono text-fg">
          /{command.name}
          {command.usage ? ` ${command.usage}` : ""}
        </span>
        {/* 不接受参数的命令：值位单独一个破折号，而不是整行消失（那会像漏渲染）。 */}
        {!command.usage && <span className="text-fg-muted">{EM_DASH}</span>}
      </div>
      <div className="flex gap-3">
        <span className="w-24 shrink-0 text-fg-muted">说明</span>
        <span className="min-w-0 break-all text-fg">{command.description}</span>
      </div>
    </div>
  );
}

export function HelpCard({
  target,
  error,
}: {
  /** `/help <target>` 的目标；缺省是完整目录。 */
  target?: string | null;
  /** 宿主代填的错误行（未知命令 `/nope` 走这里，与 `/help nope` 同形）。 */
  error?: string | null;
}) {
  const name = normalizeTarget(target);
  const command = name ? byName(name) : undefined;
  // 显式 error 优先；否则命中的 target 就是详情，未命中就是「未知命令」。
  const problem = error ?? (name && !command ? `未知命令: /${name}` : null);

  return (
    <div
      data-testid="help-card"
      className="my-2 self-stretch rounded-lg border border-line-strong bg-surface px-3 py-2 text-sm"
    >
      <div className="mb-1 font-medium text-fg">
        {command && !problem ? `命令 /${command.name}` : "可用命令"}
      </div>
      {problem != null && <div className="mb-1 text-red-300">{problem}</div>}
      {command && !problem ? <Detail command={command} /> : <Catalog />}
    </div>
  );
}
