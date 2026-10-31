import { Icon } from "./widgets";

export type TabId = "output" | "background" | "alerts" | "mcp" | "api";

export const TABS: { id: TabId; label: string; icon: Parameters<typeof Icon>[0]["name"] }[] = [
  { id: "output", label: "Output", icon: "speaker" },
  { id: "background", label: "Background", icon: "radio" },
  { id: "alerts", label: "Alerts", icon: "bell" },
  { id: "mcp", label: "MCP Docs", icon: "layers" },
  { id: "api", label: "API Docs", icon: "book" },
];

const TAB_KEY = "unsonido-tab";

export function loadTab(): TabId {
  try {
    const saved = localStorage.getItem(TAB_KEY);
    if (saved && TABS.some((t) => t.id === saved)) return saved as TabId;
  } catch {
    // localStorage unavailable; default below.
  }
  return "output";
}

export function saveTab(tab: TabId) {
  try {
    localStorage.setItem(TAB_KEY, tab);
  } catch {}
}

/** The pill tab strip; scrolling horizontally when too narrow. */
export function TabBar({
  tab,
  onTab,
}: {
  tab: TabId;
  onTab: (t: TabId) => void;
}) {
  return (
    <nav
      aria-label="Sections"
      className="flex gap-1 overflow-x-auto rounded-2xl border border-line bg-card p-1.5 shadow-sm"
    >
      {TABS.map((t) => (
        <button
          key={t.id}
          type="button"
          aria-current={tab === t.id ? "page" : undefined}
          onClick={() => onTab(t.id)}
          className={
            "flex cursor-pointer items-center gap-1.5 rounded-xl px-3 py-2 text-sm font-medium whitespace-nowrap transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent " +
            (tab === t.id
              ? "bg-ink text-card"
              : "text-muted hover:text-ink")
          }
        >
          <Icon name={t.icon} className="h-4 w-4" />
          {t.label}
        </button>
      ))}
    </nav>
  );
}