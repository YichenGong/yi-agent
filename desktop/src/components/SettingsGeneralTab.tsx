import type { Theme } from "../lib/theme";

const CHOICES: Array<{ value: Theme; label: string }> = [
  { value: "dark", label: "深色" },
  { value: "light", label: "浅色" },
];

export function SettingsGeneralTab({
  theme,
  onThemeChange,
}: {
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
}) {
  return (
    <section className="p-5">
      <h2 className="text-sm font-medium text-fg">主题</h2>
      <div className="mt-3 inline-flex rounded-md border border-line overflow-hidden">
        {CHOICES.map((c) => {
          const active = c.value === theme;
          return (
            <button
              key={c.value}
              type="button"
              aria-pressed={active}
              onClick={() => onThemeChange(c.value)}
              className={`px-4 py-1.5 text-sm ${
                active ? "bg-raised text-fg" : "text-fg-muted hover:bg-raised/50"
              }`}
            >
              {c.label}
            </button>
          );
        })}
      </div>
      <p className="mt-4 text-xs text-fg-subtle">
        也可以直接对话让 Yi-Agent 切换主题，例如「帮我切成浅色」。
      </p>
    </section>
  );
}
