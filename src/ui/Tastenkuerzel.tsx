/** Alle Tastenkürzel auf einen Blick — ⌘/ bzw. Strg+/. */

import { useEffect } from "react";
import { X } from "lucide-react";

import { t } from "../core/index.ts";
import { IS_MAC } from "./util.ts";

const A = IS_MAC ? "⌥" : "Alt";

/** Zur Zeit des Zeichnens gebaut, damit die Texte der aktuellen Sprache folgen. */
function gruppen(): Array<{ titel: string; tasten: Array<[string, string]> }> {
  const M = IS_MAC ? "⌘" : t("Strg");
  return [
    {
      titel: t("Chat"),
      tasten: [
        [`${M} N`, t("Neuer Chat")],
        ["Enter", t("Senden")],
        [`${t("Umschalt")} Enter`, t("Neue Zeile")],
        [`${A} ↑ / ${A} ↓`, t("Zur vorigen / nächsten Frage springen")],
        ["Esc", t("Menü oder Dialog schließen")],
      ],
    },
    {
      titel: t("Seitenleiste"),
      tasten: [
        [`${M} ⇧ E`, t("Dateien")],
        [`${M} ⇧ D`, t("Änderungen")],
        [`${t("Strg")} \``, "Terminal"],
        [`${M} ⇧ B`, "Browser"],
        [`${M} ⇧ H`, t("jichi-Dokumentation")],
        [`${M} ⇧ L`, t("Live-Gespräch")],
      [`${M} \\`, t("Vorderes Fenster schließen")],
      ],
    },
    {
      titel: t("Datei bearbeiten"),
      tasten: [
        [`${M} S`, t("Speichern")],
        ["Tab", t("Einrücken")],
      ],
    },
    {
      titel: t("Hilfe"),
      tasten: [[`${M} /`, t("Diese Übersicht")]],
    },
  ];
}

export function Tastenkuerzel({ schliessen }: { schliessen: () => void }) {
  useEffect(() => {
    const f = (e: KeyboardEvent) => e.key === "Escape" && schliessen();
    window.addEventListener("keydown", f);
    return () => window.removeEventListener("keydown", f);
  }, [schliessen]);
  return (
    <div className="ueber" onMouseDown={(e) => e.target === e.currentTarget && schliessen()}>
      <div className="tafel tasten-tafel" role="dialog" aria-modal="true" aria-labelledby="tasten-titel">
        <div className="tasten-kopf">
          <h2 id="tasten-titel">{t("Tastenkürzel")}</h2>
          <button type="button" className="knopf-klein knopf-symbol" aria-label={t("Schließen")} onClick={schliessen}><X size={14} /></button>
        </div>
        <div className="tasten-gruppen">
          {gruppen().map((g) => (
            <section key={g.titel}>
              <h3>{g.titel}</h3>
              <dl>
                {g.tasten.map(([k, was]) => (
                  <div key={k}>
                    <dt>{k.split(" ").map((x, i) => (x === "/" ? <span key={i} className="tasten-oder">/</span> : <kbd key={i}>{x}</kbd>))}</dt>
                    <dd>{was}</dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}
