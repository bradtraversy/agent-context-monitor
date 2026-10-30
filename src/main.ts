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
  stale: boolean;
  action: "sign_in" | null;
}

interface ContextMeasure {
  used_tokens: number;
  context_window: number | null;
  used_percent: number | null;
  confidence: "confirmed" | "estimated";
  observed_at: string;
  limited: boolean;
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
  context_limit: number | null;
  thresholds: { warning: number; critical: number };
}

interface Bar {
  start: number;
  used: number | null;
  current: boolean;
}

interface ProviderHistory {
  provider: string;
  name: string;
  daily: Bar[];
  weekly: Bar[];
}

type Range = "daily" | "weekly";


const usagePanel = document.querySelector<HTMLElement>("#panel-usage")!;
const chatList = document.querySelector<HTMLElement>("#chat-list")!;
const limitPicker = document.querySelector<HTMLSelectElement>("#context-limit")!;
const filterButtons = [...document.querySelectorAll<HTMLButtonElement>(".filter [data-provider]")];
const PROVIDER_LABEL: Record<string, string> = { claude: "Claude", codex: "Codex" };
let providerFilter = localStorage.getItem("providerFilter") ?? "all";
const historyList = document.querySelector<HTMLElement>("#history-list")!;
const historyPanel = document.querySelector<HTMLElement>("#panel-history")!;
const rangeButtons = [...document.querySelectorAll<HTMLButtonElement>(".filter [data-range]")];
let range: Range = localStorage.getItem("historyRange") === "weekly" ? "weekly" : "daily";
let history: ProviderHistory[] = [];
// Widening the window shows every panel at once, with longer history.
const wide = matchMedia("(min-width: 760px)");
// Side by side, a long chat list pushes the history chart out of sight, so it starts short.
const WIDE_CHAT_LIMIT = 3;
let showAllChats = false;
const tabs = [...document.querySelectorAll<HTMLButtonElement>('[role="tab"]')];

let snapshot: Snapshot | null = null;
// Why the last Sign in click couldn't open a terminal, by provider.
const signInErrors = new Map<string, string>();

// The same thresholds as the tray and alerts, which the app sends with each snapshot.
function levelFor(used: number): Level {
  const { warning, critical } = snapshot?.thresholds ?? { warning: 70, critical: 90 };
  if (used >= critical) return "critical";
  if (used >= warning) return "warning";
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
  // A plan without this limit (e.g. Codex Pro has no 5-hour window) gets no row at all.
  if (window.state === "not_reported") return "";
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
  const age = u.observed_at ? ago(Date.parse(u.observed_at) / 1000) : "";
  const observed = u.stale
    ? `<strong class="tag">Last reading ${age}, may be out of date</strong>`
    : age
      ? `Updated ${age}`
      : "";
  const body =
    u.status === "available"
      ? limitRow("Current session", u.session) + limitRow("This week", u.weekly)
      : "";
  const note = u.note ? `<p class="note">${escape(u.note)}</p>` : "";
  const error = signInErrors.get(u.provider);
  const action =
    u.action === "sign_in"
      ? `<button class="action" data-sign-in="${escape(u.provider)}">Sign in</button>` +
        (error ? `<p class="note">${escape(error)}</p>` : "")
      : "";
  return `<article class="card" data-stale="${u.stale}">
    <h2>${escape(u.name)} ${plan}</h2>
    ${body}${note}${action}
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
    const over = m.used_tokens - m.context_window;
    const tag = over > 0 ? (m.limited ? "Over your limit" : "Over the window") : LEVEL_TEXT[level];
    const warn = tag ? ` <strong class="tag">${tag}</strong>` : "";
    const headline = over > 0 ? `${over.toLocaleString()} tokens over` : `${left.toFixed(0)}% left`;
    const estimated = m.limited ? " (your limit)" : m.confidence === "estimated" ? " (estimated)" : "";
    gauge = `<div class="limit-head"><span>Context</span>
        <span><b>${headline}</b>${warn}</span></div>
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

let usageHtml = "";

function render() {
  // The first check takes a few seconds, and until then the page keeps its loading spinners.
  if (!snapshot?.usage.length) return;
  // Rebuilding unchanged cards would swallow a click on Sign in that spans a refresh.
  const html = snapshot.usage.map(usageCard).join("");
  if (html !== usageHtml) usagePanel.innerHTML = usageHtml = html;
  if (document.activeElement !== limitPicker) limitPicker.value = String(snapshot.context_limit ?? "");
  const chats = snapshot.chats.filter((c) => providerFilter === "all" || c.provider === providerFilter);
  const who = PROVIDER_LABEL[providerFilter] ?? "Claude or Codex";
  const capped = wide.matches && !showAllChats && chats.length > WIDE_CHAT_LIMIT + 1;
  const shown = capped ? chats.slice(0, WIDE_CHAT_LIMIT) : chats;
  const more =
    wide.matches && chats.length > WIDE_CHAT_LIMIT + 1
      ? `<button class="more">${capped ? `Show ${chats.length - WIDE_CHAT_LIMIT} more` : "Show fewer"}</button>`
      : "";
  chatList.innerHTML =
    (shown.map(chatRow).join("") || `<p class="empty">No ${who} chats in the last 24 hours.</p>`) + more;
}

usagePanel.addEventListener("click", (e) => {
  const button = (e.target as HTMLElement).closest<HTMLButtonElement>("[data-sign-in]");
  if (!button) return;
  const provider = button.dataset.signIn!;
  // The card fills in by itself once the sign-in finishes.
  invoke("sign_in", { provider })
    .then(() => signInErrors.delete(provider))
    .catch((error) => signInErrors.set(provider, String(error)))
    .finally(render);
});

chatList.addEventListener("click", (e) => {
  if (!(e.target as HTMLElement).closest(".more")) return;
  showAllChats = !showAllChats;
  render();
});

function setFilter(provider: string) {
  providerFilter = provider;
  localStorage.setItem("providerFilter", provider);
  for (const b of filterButtons) b.setAttribute("aria-checked", String(b.dataset.provider === provider));
  render();
}

for (const b of filterButtons) b.addEventListener("click", () => setFilter(b.dataset.provider!));
limitPicker.addEventListener("change", () => {
  const tokens = limitPicker.value ? Number(limitPicker.value) : null;
  invoke("set_context_limit", { tokens });
});
setFilter(providerFilter);

function barLabel(b: Bar): string {
  const date = new Date(b.start * 1000);
  if (range === "daily") {
    const day = date.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
    return b.current ? `Today (${day})` : day;
  }
  const week = `Week of ${date.toLocaleDateString([], { month: "short", day: "numeric" })}`;
  return b.current ? `${week}, so far` : week;
}

function barValue(b: Bar): string {
  if (b.used === null) return "no readings yet";
  return range === "daily" ? `${b.used.toFixed(0)}% of the weekly limit used` : `${b.used.toFixed(0)}% of the limit used`;
}

function historyCard(h: ProviderHistory): string {
  const bars = range === "daily" ? h.daily : h.weekly;
  const known = bars.filter((b) => b.used !== null);
  if (!known.length) {
    return `<article class="card"><h2>${escape(h.name)}</h2>
      <p class="note">No readings yet. Bars fill in as the app checks your usage.</p></article>`;
  }
  // Weekly bars read against the full limit; daily ones scale up so small days stay visible.
  const max = range === "weekly" ? 100 : Math.max(10, ...known.map((b) => b.used!));
  const cols = bars
    .map((b, i) => {
      const height = b.used === null ? 0 : Math.max(b.used > 0 ? 2 : 0, (b.used / max) * 100);
      const level = range === "weekly" && b.used !== null ? levelFor(b.used) : "accent";
      const date = new Date(b.start * 1000);
      // A month of daily bars is too tight to label each one, so label every seventh back from today.
      const tick =
        range === "weekly"
          ? date.toLocaleDateString([], { month: "numeric", day: "numeric" })
          : !wide.matches
            ? date.toLocaleDateString([], { weekday: "narrow" })
            : (bars.length - 1 - i) % 7 === 0
              ? date.toLocaleDateString([], { month: "short", day: "numeric" })
              : "";
      const text = `${barLabel(b)}: ${barValue(b)}`;
      return `<button class="col" data-index="${i}" data-empty="${b.used === null}" data-current="${b.current}"
          aria-label="${escape(text)}" title="${escape(text)}">
        <span class="col-track"><span class="col-fill" data-level="${level}" style="height:${height}%"></span></span>
        <span class="tick">${tick}</span>
      </button>`;
    })
    .join("");
  const latest = known[known.length - 1];
  const summary = `${barLabel(latest)}: ${barValue(latest)}`;
  return `<article class="card history" data-provider="${escape(h.provider)}">
    <h2>${escape(h.name)}</h2>
    <div class="chart" style="--cols:${bars.length}">${cols}</div>
    <div class="sub readout" data-default="${escape(summary)}">${escape(summary)}</div>
  </article>`;
}

function renderHistory() {
  const intro =
    range === "daily"
      ? "How much of each weekly limit you used per day."
      : "The most of each weekly limit you used before it reset.";
  historyList.innerHTML =
    `<p class="sub intro">${intro}</p>` +
    (history.map(historyCard).join("") || `<p class="empty">Usage history appears once usage is checked.</p>`);
}

let historyRequest = 0;

async function loadHistory() {
  if (historyPanel.hidden && !wide.matches) return;
  // Only the latest request may draw, so a slow reply for the other view can't win.
  const request = ++historyRequest;
  const loaded = await invoke<ProviderHistory[]>("get_history", {
    utcOffsetMinutes: -new Date().getTimezoneOffset(),
    days: wide.matches ? 30 : 14,
    weeks: wide.matches ? 12 : 8,
  });
  if (request !== historyRequest) return;
  history = loaded;
  renderHistory();
}

function setRange(next: Range) {
  range = next;
  localStorage.setItem("historyRange", next);
  for (const b of rangeButtons) b.setAttribute("aria-checked", String(b.dataset.range === next));
  renderHistory();
}

for (const b of rangeButtons) b.addEventListener("click", () => setRange(b.dataset.range as Range));
setRange(range);

function showReadout(target: EventTarget | null) {
  const col = (target as HTMLElement | null)?.closest<HTMLElement>(".col");
  const card = (target as HTMLElement | null)?.closest<HTMLElement>(".history");
  const readout = card?.querySelector<HTMLElement>(".readout");
  if (!readout) return;
  readout.textContent = col ? col.getAttribute("aria-label") : readout.dataset.default!;
}

historyList.addEventListener("pointerover", (e) => showReadout(e.target));
historyList.addEventListener("focusin", (e) => showReadout(e.target));
historyList.addEventListener("pointerleave", () => {
  for (const r of historyList.querySelectorAll<HTMLElement>(".readout")) r.textContent = r.dataset.default!;
});

function selectTab(tab: HTMLButtonElement) {
  for (const t of tabs) {
    const selected = t === tab;
    t.setAttribute("aria-selected", String(selected));
    t.tabIndex = selected ? 0 : -1;
    document.getElementById(t.getAttribute("aria-controls")!)!.hidden = !selected;
  }
  loadHistory();
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

function applySnapshot(next: Snapshot) {
  // History lists the providers usage was checked for, so load it once the first check lands.
  const firstUsage = !snapshot?.usage.length && next.usage.length > 0;
  snapshot = next;
  render();
  if (firstUsage) loadHistory();
}

let gotEvent = false;
listen<Snapshot>("snapshot", (event) => {
  gotEvent = true;
  applySnapshot(event.payload);
});
invoke<Snapshot>("get_snapshot").then((s) => {
  // A snapshot event that arrived first is newer than this reply.
  if (!gotEvent) applySnapshot(s);
});
// Relative times drift between snapshots, so redraw on a timer too.
setInterval(render, 30_000);
// New readings arrive every few minutes at most, so a slow refresh keeps the chart current.
setInterval(loadHistory, 60_000);
const viewToggle = document.querySelector<HTMLButtonElement>("#view-toggle")!;
// Page widths either side of the 760px breakpoint where every panel shows at once.
const COMPACT_WIDTH = 380;
const WIDE_WIDTH = 860;

function labelViewToggle() {
  const label = wide.matches ? "Compact view" : "Wide view";
  viewToggle.setAttribute("aria-label", label);
  viewToggle.title = label;
}

viewToggle.addEventListener("click", () => {
  // Going back to compact restores whatever width the window had before it went wide.
  let width = Number(localStorage.getItem("compactWidth")) || COMPACT_WIDTH;
  if (!wide.matches) {
    localStorage.setItem("compactWidth", String(innerWidth));
    width = WIDE_WIDTH;
  }
  invoke("resize_window", { width, pageWidth: innerWidth, pageHeight: innerHeight });
});

// Tall enough for three usage cards, since more providers are coming. Set here rather than only in
// the window config because large text settings zoom the page, which shrinks it in the window.
const openHeight = Math.min(720, screen.availHeight - 60);
if (innerHeight < openHeight && !sessionStorage.getItem("sized")) {
  sessionStorage.setItem("sized", "1");
  invoke("resize_window", { height: openHeight, pageWidth: innerWidth, pageHeight: innerHeight });
}

labelViewToggle();
wide.addEventListener("change", () => {
  labelViewToggle();
  render();
  loadHistory();
});
loadHistory();
