/**
 * Die Seitenleiste rechts: Reiter oben, darunter das vordere Fenster.
 *
 * Alle Fenster bleiben eingehängt, auch die hinteren — ein Terminal verliert
 * sonst seinen Verlauf, ein Browser seine Seite. Verborgen wird mit CSS.
 */

import { useEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { AudioLines, BookOpen, FileCode2, FolderTree, GitCompare, Globe, Maximize2, Minimize2, Plus, Sparkles, SquareTerminal, X } from "lucide-react";

import { t, type Snapshot } from "../../core/index.ts";
import { useSprache } from "../util.ts";
import { Dateien } from "./Dateien.tsx";
import { DateiAnsicht } from "./DateiAnsicht.tsx";
import { MIN_WIDTH, panel, type PanelTab } from "./store.ts";
import { Aenderungen } from "./Aenderungen.tsx";
import { TerminalFenster } from "./Terminal.tsx";
import { BrowserFenster } from "./Browser.tsx";
import { ArtefaktFenster } from "./Artefakt.tsx";
import { DokuFenster } from "./Doku.tsx";
import { LiveFenster } from "./Live.tsx";

export const usePanel = () => useSyncExternalStore(panel.subscribe, panel.getSnapshot);

function titel(tab: PanelTab): { icon: ReactNode; text: string } {
  switch (tab.kind) {
    case "dateien":
      return { icon: <FolderTree size={13} />, text: t("Dateien") };
    case "datei":
      return { icon: <FileCode2 size={13} />, text: tab.path.split("/").pop() ?? tab.path };
    case "aenderungen":
      return { icon: <GitCompare size={13} />, text: t("Änderungen") };
    case "terminal":
      return { icon: <SquareTerminal size={13} />, text: tab.agentTerminal ? "jichi" : t("Terminal") };
    case "browser":
      return { icon: <Globe size={13} />, text: tab.url ? hostOf(tab.url) : t("Browser") };
    case "artefakt":
      return { icon: <Sparkles size={13} />, text: tab.title };
    case "doku":
      return { icon: <BookOpen size={13} />, text: t("jichi-Doku") };
    case "live":
      return { icon: <AudioLines size={13} />, text: t("Live-Gespräch") };
  }
}

function hostOf(url: string): string {
  try {
    const u = new URL(url);
    return u.host || u.pathname.split("/").pop() || url;
  } catch {
    return url;
  }
}

/** Das Menü hinter „+“: welche Fenster es gibt. */
function NeuMenue() {
  const [offen, setOffen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!offen) return;
    const zu = (e: MouseEvent) => box.current && !box.current.contains(e.target as Node) && setOffen(false);
    document.addEventListener("mousedown", zu);
    return () => document.removeEventListener("mousedown", zu);
  }, [offen]);
  const kurz = /Mac/i.test(navigator.userAgent) ? "⌘" : `${t("Strg")}+`;
  const punkt = (icon: ReactNode, text: string, taste: string, f: () => void) => (
    <button type="button" className="menue-punkt" role="menuitem" onClick={() => { setOffen(false); f(); }}>
      <span className="menue-icon">{icon}</span>
      <span className="menue-titel">{text}</span>
      <span className="menue-text menue-taste">{taste}</span>
    </button>
  );
  return (
    <div className="panel-neu" ref={box}>
      <button type="button" className="panel-knopf" aria-label={t("Fenster öffnen")} title={t("Fenster öffnen")} onClick={() => setOffen((o) => !o)}>
        <Plus size={14} />
      </button>
      {offen && (
        <div className="menue panel-menue" role="menu">
          {punkt(<FolderTree size={15} />, t("Dateien"), `${kurz}⇧E`, () => panel.dateien())}
          {punkt(<GitCompare size={15} />, t("Änderungen"), `${kurz}⇧D`, () => panel.aenderungen())}
          {punkt(<SquareTerminal size={15} />, t("Terminal"), `${t("Strg")}+\``, () => panel.terminal(undefined, true))}
          {punkt(<Globe size={15} />, t("Browser"), `${kurz}⇧B`, () => panel.browser())}
          {punkt(<BookOpen size={15} />, t("jichi-Dokumentation"), `${kurz}⇧H`, () => panel.doku())}
          {punkt(<AudioLines size={15} />, t("Live-Gespräch"), `${kurz}⇧L`, () => panel.live())}
        </div>
      )}
    </div>
  );
}

export function Panel({ snap }: { snap: Snapshot }) {
  useSprache();
  const p = usePanel();
  const [zieht, setZieht] = useState(false);

  function ziehen(e: React.MouseEvent) {
    e.preventDefault();
    const start = e.clientX;
    const breite = p.width;
    setZieht(true);
    const bewege = (m: MouseEvent) => {
      const max = Math.max(MIN_WIDTH, window.innerWidth - 480);
      panel.setWidth(Math.min(max, breite + (start - m.clientX)));
    };
    const los = () => {
      setZieht(false);
      window.removeEventListener("mousemove", bewege);
      window.removeEventListener("mouseup", los);
    };
    window.addEventListener("mousemove", bewege);
    window.addEventListener("mouseup", los);
  }

  if (!p.open) return null;

  return (
    <aside className={`panel${zieht ? " zieht" : ""}${p.maximized ? " gross" : ""}`} aria-label={t("Seitenleiste")}>
      {!p.maximized && <div className="panel-griff" onMouseDown={ziehen} onDoubleClick={() => panel.setWidth(560)} title={t("Breite ziehen")} />}
      <div className="panel-kopf" data-tauri-drag-region>
        <div className="panel-reiter" role="tablist">
          {p.tabs.map((tab) => {
            const { icon, text } = titel(tab);
            const aktiv = tab.id === p.active;
            return (
              <div key={tab.id} className={`panel-tab${aktiv ? " aktiv" : ""}`} role="tab" aria-selected={aktiv}
                onMouseDown={(e) => e.button === 1 && (e.preventDefault(), panel.close(tab.id))}>
                <button type="button" className="panel-tab-titel" onClick={() => panel.activate(tab.id)} title={tab.kind === "datei" ? tab.path : tab.kind === "browser" ? tab.url : text}>
                  {icon}
                  <span>{text}</span>
                </button>
                <button type="button" className="panel-tab-zu" aria-label={t("{name} schließen", { name: text })} onClick={() => panel.close(tab.id)}>
                  <X size={11} />
                </button>
              </div>
            );
          })}
        </div>
        <NeuMenue />
        <button type="button" className="panel-knopf" aria-label={p.maximized ? t("Verkleinern") : t("Vergrößern")} title={p.maximized ? t("Verkleinern") : t("Vergrößern")} onClick={() => panel.toggleMaximized()}>
          {p.maximized ? <Minimize2 size={13} /> : <Maximize2 size={13} />}
        </button>
        <button type="button" className="panel-knopf" aria-label={t("Seitenleiste schließen")} title={t("Schließen")} onClick={() => panel.hide()}>
          <X size={14} />
        </button>
      </div>
      <div className="panel-inhalt">
        {p.tabs.map((tab) => {
          const sichtbar = tab.id === p.active;
          return (
            <div key={tab.id} className="panel-fenster" hidden={!sichtbar}>
              {tab.kind === "dateien" && <Dateien snap={snap} />}
              {tab.kind === "datei" && <DateiAnsicht path={tab.path} line={tab.line} sichtbar={sichtbar} />}
              {tab.kind === "aenderungen" && <Aenderungen snap={snap} sichtbar={sichtbar} />}
              {tab.kind === "terminal" && <TerminalFenster tab={tab} snap={snap} sichtbar={sichtbar} />}
              {tab.kind === "browser" && <BrowserFenster tab={tab} sichtbar={sichtbar && !p.overlay && !zieht} />}
              {tab.kind === "artefakt" && <ArtefaktFenster tab={tab} />}
              {tab.kind === "doku" && <DokuFenster tab={tab} snap={snap} />}
              {tab.kind === "live" && <LiveFenster />}
            </div>
          );
        })}
      </div>
    </aside>
  );
}
