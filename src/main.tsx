/**
 * jichi Desktop — Oberfläche.
 *
 * Gebaut aus den Farben, Schriften und Radien des JLU Design System, aber mit
 * eigenem Gerüst: dessen Komponenten folgen Material 3 und sind für den Finger
 * gerastert, was auf dem Schreibtisch wie eine Fernsehoberfläche wirkt. Die
 * Maße stehen darum in `styles.css` an einer Stelle.
 *
 * Diese Datei kennt kein ACP, keinen Prozess und keinen Schlüssel. Sie liest
 * einen Schnappschuss und ruft Methoden — der Vertrag steht in
 * `docs/CONTRACT.md`.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  AudioLines,
  BookOpen,
  FolderOpen,
  FolderTree,
  GitCompare,
  Globe,
  Moon,
  PanelRight,
  SquareTerminal,
  Sun,
} from "lucide-react";
import {
  agent,
  applyAppearance,
  readPreferences,
  setSprache,
  shortPath,
  statusLabel,
  t,
  watchAppearance,
  writePreferences,
  type Preferences,
} from "./core/index.ts";
import { Eingabe } from "./ui/Eingabe.tsx";
import { ChatMenue } from "./ui/ChatMenue.tsx";
import { Einrichtung } from "./ui/Einrichtung.tsx";
import { Tastenkuerzel } from "./ui/Tastenkuerzel.tsx";
import { ProjektEinrichten } from "./ui/ProjektEinrichten.tsx";
import { Einstellungen } from "./ui/Einstellungen.tsx";
import { Seitenleiste } from "./ui/Seitenleiste.tsx";
import { Leer, Verlauf } from "./ui/Verlauf.tsx";
import { Panel, usePanel } from "./ui/panel/Panel.tsx";
import { panel } from "./ui/panel/store.ts";
import { IS_MAC, kurzTaste, useAgent, useSprache } from "./ui/util.ts";
import "./styles.css";

// ── Anwendung ────────────────────────────────────────────────────────────────

function App() {
  const snap = useAgent();
  useSprache();
  const [prefs, setPrefsState] = useState<Preferences>(() => readPreferences());
  const [einstellungen, setEinstellungen] = useState(false);
  const [tastenOffen, setTastenOffen] = useState(false);

  const setPrefs = useCallback((next: Preferences) => {
    setPrefsState(next);
    writePreferences(next);
    applyAppearance(next.appearance);
    setSprache(next.language);
  }, []);

  useEffect(() => {
    void agent.init();
  }, []);

  // Der Systemeinstellung folgen, solange "System" gewählt ist.
  const wahl = useRef(prefs.appearance);
  wahl.current = prefs.appearance;
  useEffect(() => watchAppearance(() => wahl.current), []);

  useEffect(() => {
    function tasten(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key === "n") {
        e.preventDefault();
        if (!agent.getSnapshot().canSwitch) return;
        void agent.newSession().catch(() => {});
      }
      // Seitenleiste — dieselben Tasten wie in Claude Code.
      const k = e.key.toLowerCase();
      if (mod && e.shiftKey && k === "e") { e.preventDefault(); panel.dateien(); }
      if (mod && e.shiftKey && k === "d") { e.preventDefault(); panel.aenderungen(); }
      if (mod && e.shiftKey && k === "b") { e.preventDefault(); panel.browser(); }
      if (mod && e.shiftKey && k === "h") { e.preventDefault(); panel.doku(); }
      if (mod && e.shiftKey && k === "l") { e.preventDefault(); panel.live(); }
      if (e.ctrlKey && (e.key === "`" || e.code === "Backquote")) { e.preventDefault(); panel.terminal(); }
      if (mod && e.key === "\\") { e.preventDefault(); panel.closeActive(); }
      if (e.key === "Escape") setEinstellungen(false);
      if (mod && e.key === "/") { e.preventDefault(); setTastenOffen((o) => !o); }
    }
    window.addEventListener("keydown", tasten);
    return () => window.removeEventListener("keydown", tasten);
  }, []);

  // Wird die Einrichtung nötig (Schlüssel entfernt), schliessen die Einstellungen
  // — sonst sprängen sie danach ungefragt wieder auf.
  useEffect(() => {
    if (snap.needsSetup) setEinstellungen(false);
  }, [snap.needsSetup]);

  // Ein Dialog über allem: der native Browser der Seitenleiste muss weichen.
  useEffect(() => {
    panel.setOverlay(einstellungen || snap.needsSetup || tastenOffen);
  }, [einstellungen, snap.needsSetup, tastenOffen]);
  const p = usePanel();

  const punkt =
    snap.status === "ready"
      ? "bereit"
      : snap.status === "busy" || snap.status === "starting" || snap.status === "cancelling"
        ? "aktiv"
        : snap.status === "error"
          ? "fehler"
          : "";

  return (
    <>
      <div
        className={`app layout-${prefs.layout}${p.open ? " mit-panel" : ""}${p.open && p.maximized ? " panel-gross" : ""}`}
        style={{ ["--panel-breite" as string]: `${p.width}px` }}
        inert={snap.needsSetup}
        aria-hidden={snap.needsSetup}
      >
      <Seitenleiste snap={snap} oeffneEinstellungen={() => setEinstellungen(true)} />

      <main className="haupt">
        <header className="kopf" data-tauri-drag-region>
          <div className="zustand" data-tauri-drag-region>
            <span className={`punkt ${punkt}`} data-tauri-drag-region />
            {snap.error ?? statusLabel(snap.status)}
          </div>

          <div className="kopf-rechts" data-tauri-drag-region>
            <ChatMenue snap={snap} tasten={() => setTastenOffen(true)} />
            <button
              className="knopf-klein"
              disabled={!snap.canSwitch}
              onClick={() => void agent.pickWorkspace().catch(() => {})}
              title={snap.hasProject && snap.cwd ? snap.cwd : t("Projektordner wählen")}
            >
              <FolderOpen size={13} />
              <span className="pfad">{snap.hasProject ? shortPath(snap.cwd, 34) : t("Projekt öffnen")}</span>
            </button>
            <div className="kopf-ansichten" role="group" aria-label={t("Seitenleiste")}>
              <button className="knopf-klein knopf-symbol" onClick={() => panel.dateien()} aria-label={t("Dateien")} title={`${t("Dateien")} (${kurzTaste("⇧E")})`}><FolderTree size={14} /></button>
              <button className="knopf-klein knopf-symbol" onClick={() => panel.aenderungen()} aria-label={t("Änderungen")} title={`${t("Änderungen")} (${kurzTaste("⇧D")})`}><GitCompare size={14} /></button>
              <button className="knopf-klein knopf-symbol" onClick={() => panel.terminal()} aria-label="Terminal" title={`Terminal (${t("Strg")}+\`)`}><SquareTerminal size={14} /></button>
              <button className="knopf-klein knopf-symbol" onClick={() => panel.browser()} aria-label="Browser" title={`Browser (${kurzTaste("⇧B")})`}><Globe size={14} /></button>
              <button className="knopf-klein knopf-symbol" onClick={() => panel.doku()} aria-label={t("jichi-Dokumentation")} title={`${t("jichi-Dokumentation")} (${kurzTaste("⇧H")})`}><BookOpen size={14} /></button>
              <button className="knopf-klein knopf-symbol" onClick={() => panel.live()} aria-label={t("Live-Gespräch")} title={`${t("Live-Gespräch")} (${kurzTaste("⇧L")})`}><AudioLines size={14} /></button>
              <button className={`knopf-klein knopf-symbol${p.open ? " an" : ""}`} onClick={() => panel.toggle()} aria-label={t("Seitenleiste ein/aus")} aria-pressed={p.open} title={t("Seitenleiste")}><PanelRight size={14} /></button>
            </div>
            <button
              className="knopf-klein knopf-symbol"
              onClick={() =>
                setPrefs({
                  ...prefs,
                  appearance: prefs.appearance === "dark" ? "light" : "dark",
                })
              }
              aria-label={t("Erscheinungsbild wechseln")}
            >
              {prefs.appearance === "dark" ? <Sun size={14} /> : <Moon size={14} />}
            </button>
          </div>
        </header>

        {snap.transcript.length === 0 && !snap.permission ? (
          <Leer snap={snap} name={prefs.name} frage={(text) => void agent.send(text).catch(() => {})} />
        ) : (
          <Verlauf snap={snap} />
        )}

        <Eingabe snap={snap} />
      </main>

      <Panel snap={snap} />
      {tastenOffen && <Tastenkuerzel schliessen={() => setTastenOffen(false)} />}

      {einstellungen && !snap.needsSetup && (
        <Einstellungen
          snap={snap}
          prefs={prefs}
          setPrefs={setPrefs}
          schliessen={() => setEinstellungen(false)}
        />
      )}
      </div>
      {snap.needsSetup && <Einrichtung snap={snap} prefs={prefs} setPrefs={setPrefs} />}
      <ProjektEinrichten snap={snap} />
    </>
  );
}

// Die Sprache steht vor dem ersten Zeichnen fest, nicht erst danach.
setSprache(readPreferences().language);

// Nur auf macOS schwebt die Ampel über dem Inhalt und braucht Platz.
if (IS_MAC) document.documentElement.classList.add("mac");

const wurzel = document.getElementById("root");
if (wurzel) createRoot(wurzel).render(<App />);
