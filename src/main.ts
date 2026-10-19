import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type Level = "unknown" | "ok" | "warning" | "critical";

type LimitWindow =
  | { state: "reported"; used_percent: number; resets_at: number | null }
  | { state: "reset_since_reading"; resets_at: number }
  | { state: "not_reported" };

interface ProviderUsage {
  provider: string;
  name: string;
  status: "available" | "unknown";
  note: string | null;
  plan: string | null;
  observed_at: string | null;
  session: LimitWindow;
  weekly: LimitWindow;
}

interface ContextMeasure {
  used_tokens: number;
  context_window: number | null;
  used_percent: number | null;
  confidence: "confirmed" | "estimated";
  observed_at: string;
}

interface ChatContext {
  provider: string;
  id: string;
  title: string | null;
  project: string | null;
  cwd: string | null;
  model: string | null;
  context: ContextMeasure | null;
  last_activity: number;
  subagents: number;
  activity: "working" | "idle" | null;
  surface: string | null;
}

interface Snapshot {
  usage: ProviderUsage[];
  chats: ChatContext[];
  level: Level;
}

const WARNING = 70;
const CRITICAL = 90;

const usagePanel = document.querySelector<HTMLElement>("#panel-usage")!;
const chatList = document.querySelector<HTMLElement>("#chat-list")!;
const filterButtons = [...document.querySelectorAll<HTMLButtonElement>(".filter [data-provider]")];
const PROVIDER_LABEL: Record<string, string> = { claude: "Claude", codex: "Codex" };
let providerFilter = localStorage.getItem("providerFilter") ?? "all";
const tabs = [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')];

let snapshot: Snapshot | null = null;

function levelFor(used: number): Level {
  if (used >= CRITICAL) return "critical";
  if (used >= WARNING) return "warning";
  return "ok";
}

const LEVEL_TEXT: Record<Level, string> = {
  unknown: "",
  ok: "",
  warning: "Running low",
  critical: "Almost out",
};

function escape(text: string): string {
  return text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

function ago(unixSeconds: number): string {
  const secs = Math.max(0, Date.now() / 1000 - unixSeconds);
  if (secs < 60) return "just now";
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
}

function resetText(unixSeconds: number): string {
  const date = new Date(unixSeconds * 1000);
  const secs = unixSeconds - Date.now() / 1000;
  const time = date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (secs < 0) return `reset ${time}`;
  const hours = Math.floor(secs / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  const countdown = hours >= 24 ? `${Math.floor(hours / 24)}d ${hours % 24}h` : `${hours}h ${mins}m`;
  const sameDay = date.toDateString() === new Date().toDateString();
  const when = sameDay ? time : `${date.toLocaleDateString([], { weekday: "short" })} ${time}`;
  return `resets ${when} (in ${countdown})`;
}

function bar(used: number, level: Level): string {
  const pct = Math.min(100, Math.max(0, used));
  return `<div class="bar" data-level="${level}"><div class="fill" style="width:${pct}%"></div></div>`;
}

function limitRow(label: string, window: LimitWindow): string {
  if (window.state === "not_reported") {
    return `<div class="limit muted"><div class="limit-head"><span>${label}</span><span>Not reported</span></div></div>`;
  }
  if (window.state === "reset_since_reading") {
    return `<div class="limit muted">
      <div class="limit-head"><span>${label}</span><span>Reset, current value unknown</span></div>
      ${bar(0, "unknown")}
      <div class="sub">${resetText(window.resets_at)}</div>
    </div>`;
  }
  const level = levelFor(window.used_percent);
  const left = Math.max(0, 100 - window.used_percent);
  const warn = LEVEL_TEXT[level] ? ` <strong class="tag">${LEVEL_TEXT[level]}</strong>` : "";
  return `<div class="limit">
    <div class="limit-head"><span>${label}</span><span><b>${left.toFixed(0)}% left</b>${warn}</span></div>
    ${bar(window.used_percent, level)}
    <div class="sub">${window.resets_at ? resetText(window.resets_at) : "Reset time unknown"}</div>
  </div>`;
}

function usageCard(u: ProviderUsage): string {
  const plan = u.plan ? `<span class="pill">${escape(u.plan)}</span>` : "";
  const observed = u.observed_at ? `Updated ${ago(Date.parse(u.observed_at) / 1000)}` : "";
  const body =
    u.status === "available"
      ? limitRow("Current session", u.session) + limitRow("This week", u.weekly)
      : "";
  const note = u.note ? `<p class="note">${escape(u.note)}</p>` : "";
  return `<article class="card">
    <h2>${escape(u.name)} ${plan}</h2>
    ${body}${note}
    <div class="meta">${observed}</div>
  </article>`;
}

function chatRow(c: ChatContext): string {
  const title = c.title ?? c.project ?? `Session ${c.id.slice(-8)}`;
  const project = c.project ? `<span>${escape(c.project)}</span>` : "";
  const surface = c.surface ? `<span>${escape(c.surface)}</span>` : "";
  const model = c.model ? `<span>${escape(c.model)}</span>` : "";
  const subagents = c.subagents ? `<span>${c.subagents} subagent${c.subagents > 1 ? "s" : ""}</span>` : "";
  const recent = Date.now() / 1000 - c.last_activity < 120;
  const working = c.activity ? c.activity === "working" : recent;
  const state = `<span class="state" data-active="${working}">${working ? "Working" : "Idle"} · ${ago(c.last_activity)}</span>`;

  let gauge = `<div class="sub">Context unknown until the next reply</div>`;
  const m = c.context;
  if (m && m.used_percent !== null && m.context_window !== null) {
    const level = levelFor(m.used_percent);
    const left = Math.max(0, 100 - m.used_percent);
    const warn = LEVEL_TEXT[level] ? ` <strong class="tag">${LEVEL_TEXT[level]}</strong>` : "";
    const estimated = m.confidence === "estimated" ? " (estimated)" : "";
    gauge = `<div class="limit-head"><span>Context</span>
        <span><b>${left.toFixed(0)}% left</b>${warn}</span></div>
      ${bar(m.used_percent, level)}
      <div class="sub">${m.used_tokens.toLocaleString()} of ${m.context_window.toLocaleString()} tokens${estimated}</div>`;
  } else if (m) {
    gauge = `<div class="limit-head"><span>Context</span><span><b>${m.used_tokens.toLocaleString()} tokens</b></span></div>
      <div class="sub">Window size unknown, so no percentage</div>`;
  }

  return `<article class="card chat" title="${escape(c.cwd ?? "")}">
    <div class="chat-head"><span class="provider">${escape(c.provider)}</span><h3>${escape(title)}</h3></div>
    <div class="chips">${project}${surface}${model}${subagents}</div>
    ${gauge}
    ${state}
  </article>`;
}

function render() {
  if (!snapshot) return;
  usagePanel.innerHTML = snapshot.usage.map(usageCard).join("");
  const chats = snapshot.chats.filter((c) => providerFilter === "all" || c.provider === providerFilter);
  const who = PROVIDER_LABEL[providerFilter] ?? "Claude or Codex";
  chatList.innerHTML =
    chats.map(chatRow).join("") || `<p class="empty">No open ${who} chats right now.</p>`;
}

function setFilter(provider: string) {
  providerFilter = provider;
  localStorage.setItem("providerFilter", provider);
  for (const b of filterButtons) b.setAttribute("aria-checked", String(b.dataset.provider === provider));
  render();
}

for (const b of filterButtons) b.addEventListener("click", () => setFilter(b.dataset.provider!));
setFilter(providerFilter);

function selectTab(tab: HTMLButtonElement) {
  for (const t of tabs) {
    const selected = t === tab;
    t.setAttribute("aria-selected", String(selected));
    t.tabIndex = selected ? 0 : -1;
    document.getElementById(t.getAttribute("aria-controls")!)!.hidden = !selected;
  }
}

for (const tab of tabs) {
  tab.addEventListener("click", () => selectTab(tab));
  tab.addEventListener("keydown", (e) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const next = tabs[(tabs.indexOf(tab) + (e.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
    selectTab(next);
    next.focus();
  });
}

listen<Snapshot>("snapshot", (event) => {
  snapshot = event.payload;
  render();
});
invoke<Snapshot>("get_snapshot").then((s) => {
  snapshot = s;
  render();
});
// Relative times drift between snapshots, so redraw on a timer too.
setInterval(render, 30_000);
