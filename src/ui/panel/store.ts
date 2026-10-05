/**
 * Zustand der Seitenleiste: welche Fenster offen sind, welches vorn liegt,
 * wie breit sie ist.
 *
 * Reiner Oberflächenzustand — kein Protokoll, kein Prozess. Er hat dieselbe
 * Form wie der Agent (`subscribe`/`getSnapshot`), damit React ihn mit
 * `useSyncExternalStore` liest, und wird im `localStorage` gemerkt: wer die
 * Anwendung neu startet, findet seine Fenster wieder.
 */

export type PanelKind = "dateien" | "datei" | "aenderungen" | "terminal" | "browser" | "artefakt" | "doku" | "live";

export interface ArtefaktInhalt {
  title: string;
  /** html, svg, mermaid, markdown, … */
  lang: string;
  code: string;
}

export type PanelTab =
  | { id: string; kind: "dateien" }
  | { id: string; kind: "datei"; path: string; line?: number }
  | { id: string; kind: "aenderungen" }
  | { id: string; kind: "terminal"; agentTerminal?: string }
  | { id: string; kind: "browser"; url: string }
  /** jichis Dokumentation; `seite` relativ zu docs/, leer = Übersicht. */
  | { id: string; kind: "doku"; seite?: string; anker?: string }
  /** Live-Gespräch über eine Realtime-Bridge (Sprache rein, Sprache raus). */
  | { id: string; kind: "live" }
  | ({ id: string; kind: "artefakt" } & ArtefaktInhalt);

export interface PanelState {
  open: boolean;
  tabs: PanelTab[];
  active: string | null;
  /** Breite in Pixeln. */
  width: number;
  /** Nimmt die ganze Breite neben der Seitenleiste ein. */
  maximized: boolean;
  /** Ein Dialog liegt über allem — native Ansichten (Browser) müssen dann weichen. */
  overlay: boolean;
}

const KEY = "jichi-desktop.panel.v1";
export const MIN_WIDTH = 340;
const DEFAULT_WIDTH = 560;

function load(): PanelState {
  const leer: PanelState = { open: false, tabs: [], active: null, width: DEFAULT_WIDTH, maximized: false, overlay: false };
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? "null") as Partial<PanelState> | null;
    if (!raw || !Array.isArray(raw.tabs)) return leer;
    // Artefakte und Terminals des Agenten leben nur in der Sitzung.
    const tabs = raw.tabs.filter((t) => t && ["dateien", "datei", "aenderungen", "terminal", "browser", "doku", "live"].includes(t.kind))
      .map((t) => (t.kind === "terminal" ? { id: t.id, kind: "terminal" as const } : t));
    const active = tabs.some((t) => t.id === raw.active) ? raw.active ?? null : tabs[0]?.id ?? null;
    return {
      open: !!raw.open && tabs.length > 0,
      tabs,
      active,
      width: typeof raw.width === "number" ? Math.max(MIN_WIDTH, raw.width) : DEFAULT_WIDTH,
      maximized: !!raw.maximized,
      overlay: false,
    };
  } catch {
    return leer;
  }
}

let state = load();
const listeners = new Set<() => void>();

function set(next: Partial<PanelState>): void {
  state = { ...state, ...next };
  try {
    const { overlay: _o, ...rest } = state;
    localStorage.setItem(KEY, JSON.stringify({ ...rest, tabs: rest.tabs.filter((t) => t.kind !== "artefakt") }));
  } catch {
    /* ohne Ablage merkt sie sich nur nichts */
  }
  for (const l of listeners) l();
}

let zaehler = 0;
const neueId = () => `p${Date.now().toString(36)}${(zaehler++).toString(36)}`;

/** Gibt es dieses Fenster schon, wird es nach vorn geholt, sonst angelegt. */
function zeige(finde: (t: PanelTab) => boolean, neu: () => PanelTab, update?: (t: PanelTab) => PanelTab): void {
  const da = state.tabs.find(finde);
  if (da) {
    const tabs = update ? state.tabs.map((t) => (t.id === da.id ? update(t) : t)) : state.tabs;
    set({ open: true, active: da.id, tabs });
    return;
  }
  const tab = neu();
  set({ open: true, tabs: [...state.tabs, tab], active: tab.id });
}

export const panel = {
  subscribe(l: () => void): () => void {
    listeners.add(l);
    return () => listeners.delete(l);
  },
  getSnapshot: (): PanelState => state,

  dateien(): void {
    zeige((t) => t.kind === "dateien", () => ({ id: neueId(), kind: "dateien" }));
  },
  datei(path: string, line?: number): void {
    const p = path.replace(/^\.\//, "");
    zeige(
      (t) => t.kind === "datei" && t.path === p,
      () => ({ id: neueId(), kind: "datei", path: p, line }),
      (t) => (t.kind === "datei" ? { ...t, line } : t),
    );
  },
  /** jichis Dokumentation — die Übersicht, oder gleich eine Seite. */
  doku(seite?: string): void {
    zeige(
      (t) => t.kind === "doku",
      () => ({ id: neueId(), kind: "doku", seite }),
      (t) => (t.kind === "doku" && seite !== undefined ? { ...t, seite, anker: undefined } : t),
    );
  },
  /** Live-Gespräch: mit der Realtime-Bridge sprechen. */
  live(): void {
    zeige((t) => t.kind === "live", () => ({ id: neueId(), kind: "live" }));
  },
  aenderungen(): void {
    zeige((t) => t.kind === "aenderungen", () => ({ id: neueId(), kind: "aenderungen" }));
  },
  /** Ein neues Terminal — oder, mit Kennung, das eines Befehls von jichi. */
  terminal(agentTerminal?: string, neu = false): void {
    if (agentTerminal) {
      zeige((t) => t.kind === "terminal" && t.agentTerminal === agentTerminal, () => ({ id: neueId(), kind: "terminal", agentTerminal }));
    } else if (neu) {
      const tab: PanelTab = { id: neueId(), kind: "terminal" };
      set({ open: true, tabs: [...state.tabs, tab], active: tab.id });
    } else {
      zeige((t) => t.kind === "terminal" && !t.agentTerminal, () => ({ id: neueId(), kind: "terminal" }));
    }
  },
  browser(url = ""): void {
    const aktiv = state.tabs.find((t) => t.id === state.active);
    if (!url) {
      zeige((t) => t.kind === "browser", () => ({ id: neueId(), kind: "browser", url: "" }));
      return;
    }
    // Ein Verweis öffnet im vorderen Browser, sonst in einem neuen.
    if (aktiv?.kind === "browser") {
      set({ open: true, tabs: state.tabs.map((t) => (t.id === aktiv.id ? { ...t, url } : t)) });
      return;
    }
    zeige((t) => t.kind === "browser" && t.url === url, () => ({ id: neueId(), kind: "browser", url }));
  },
  artefakt(inhalt: ArtefaktInhalt): void {
    zeige(
      (t) => t.kind === "artefakt" && t.code === inhalt.code,
      () => ({ id: neueId(), kind: "artefakt", ...inhalt }),
    );
  },
  /** Ein Fenster aktualisieren (z. B. neue Adresse im Browser). */
  update(id: string, patch: Partial<PanelTab>): void {
    set({ tabs: state.tabs.map((t) => (t.id === id ? ({ ...t, ...patch } as PanelTab) : t)) });
  },
  activate(id: string): void {
    set({ active: id, open: true });
  },
  close(id: string): void {
    const i = state.tabs.findIndex((t) => t.id === id);
    if (i < 0) return;
    const tabs = state.tabs.filter((t) => t.id !== id);
    const active = state.active === id ? (tabs[i] ?? tabs[i - 1] ?? null)?.id ?? null : state.active;
    set({ tabs, active, open: state.open && tabs.length > 0 });
  },
  closeActive(): void {
    if (state.open && state.active) panel.close(state.active);
  },
  toggle(): void {
    if (state.open) set({ open: false });
    else if (state.tabs.length) set({ open: true });
    else panel.dateien();
  },
  hide(): void {
    set({ open: false });
  },
  setWidth(width: number): void {
    set({ width: Math.max(MIN_WIDTH, Math.round(width)) });
  },
  toggleMaximized(): void {
    set({ maximized: !state.maximized });
  },
  setOverlay(overlay: boolean): void {
    if (overlay !== state.overlay) set({ overlay });
  },
};

// ── Zum Eingabefeld ──────────────────────────────────────────────────────────
//
// Aus der Seitenleiste in die Nachricht: ein markierter Ausschnitt („Auswahl
// an jichi“) oder eine Datei als Kontext. Das Eingabefeld hört zu.

export type ZurEingabe = { text: string } | { datei: string } | { ersetzen: string } | { dateien: string[] };
const eingabeHoerer = new Set<(e: ZurEingabe) => void>();

export const zurEingabe = {
  listen(f: (e: ZurEingabe) => void): () => void {
    eingabeHoerer.add(f);
    return () => eingabeHoerer.delete(f);
  },
  send(e: ZurEingabe): void {
    for (const f of eingabeHoerer) f(e);
  },
};
