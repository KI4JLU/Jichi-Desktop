/**
 * Live-Gespräch in der Seitenleiste: mit einer Realtime-Bridge sprechen.
 *
 * Nur die Adresse der Bridge eintragen (z. B. `http://127.0.0.1:8001`); den
 * Rest — Modelle, Stimme, Szenarien, Gedächtnis — meldet die Bridge selbst.
 * Gesprochen wird über das Mikrofon (Pausen erkennt die Bridge) oder getippt.
 * Die Logik steckt in `core/realtime.ts`; hier wird nur gezeichnet.
 */

import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { Mic, MicOff, Plug, PlugZap, RotateCcw, Send } from "lucide-react";

import { live, t, type LiveSnapshot } from "../../core/index.ts";
import { useSprache } from "../util.ts";

const SPRACHEN: Array<[string, string]> = [
  ["de", "Deutsch"], ["en", "English"], ["ru", "Русский"], ["fr", "Français"],
  ["es", "Español"], ["it", "Italiano"], ["nl", "Nederlands"], ["pt", "Português"],
];

function statusText(s: LiveSnapshot): string {
  if (s.verbindung === "verbinde") return t("Verbinde …");
  if (s.verbindung !== "verbunden") return t("Nicht verbunden");
  switch (s.status) {
    case "hoert": return t("Ich höre zu …");
    case "du-sprichst": return t("Du sprichst …");
    case "pause": return t("Pause … (weiterreden geht)");
    case "denkt": return t("Denke nach …");
    case "spricht": return t("Spricht …");
    default: return t("Bereit — sprechen oder tippen");
  }
}

export function LiveFenster() {
  useSprache();
  const s = useSyncExternalStore(live.subscribe, live.snapshot);
  const [url, setUrl] = useState(s.url || "http://127.0.0.1:8001");
  const [text, setText] = useState("");
  const [kontext, setKontext] = useState("");
  const verlauf = useRef<HTMLDivElement>(null);

  const szenario = s.szenarien.find((x) => x.id === s.szenario);
  const verbunden = s.verbindung === "verbunden";

  // Beim Öffnen gleich verbinden, wenn eine Adresse bekannt ist
  useEffect(() => {
    if (s.url && s.verbindung === "aus") void live.verbinden(s.url);
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    setKontext(szenario?.hidden ? "" : (s.kontext ?? szenario?.prompt ?? ""));
  }, [s.szenario, s.kontext, szenario?.prompt, szenario?.hidden]);

  useEffect(() => {
    verlauf.current?.scrollTo({ top: verlauf.current.scrollHeight });
  }, [s.nachrichten]);

  function senden() {
    live.textSenden(text);
    setText("");
  }

  return (
    <div className="live">
      <div className="live-kopf">
        <label className="feld live-adresse">
          <span>{t("Realtime-Bridge")}</span>
          <div className="live-zeile">
            <input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && void live.verbinden(url)}
              placeholder="http://127.0.0.1:8001"
              spellCheck={false}
            />
            {verbunden ? (
              <button className="knopf" onClick={() => live.trennen()} title={t("Trennen")}><PlugZap size={14} /> {t("Trennen")}</button>
            ) : (
              <button className="knopf haupt" onClick={() => void live.verbinden(url)} disabled={s.verbindung === "verbinde"}>
                <Plug size={14} /> {t("Verbinden")}
              </button>
            )}
          </div>
        </label>

        {s.info && (
          <div className="live-modelle">
            <div>{t("Erkennung")}<b>{s.info.stt}</b></div>
            <div>{t("Modell")}<b>{s.info.llm}</b></div>
            <div>{t("Stimme")}<b>{s.info.tts} · {s.info.stimme}</b></div>
            <div>{t("Gedächtnis")}<b>{s.info.gedaechtnis === "jichi" ? t("jichi-Session") : t("nur Verbindung")}</b></div>
          </div>
        )}

        {verbunden && (
          <>
            <div className="live-zeile">
              <select value={s.szenario} onChange={(e) => live.szenarioWaehlen(e.target.value)} title={t("Wer ist die KI in diesem Gespräch?")}>
                {s.szenarien.map((x) => <option key={x.id} value={x.id}>{x.title}</option>)}
              </select>
              <button className="knopf-klein" onClick={() => live.neuesGespraech()} title={t("Gedächtnis für dieses Szenario leeren")}>
                <RotateCcw size={13} /> {t("Neues Gespräch")}
              </button>
            </div>
            {szenario?.description && <div className="live-hinweis">{szenario.description}</div>}
            <div className="live-zeile live-klein">
              <label className="schalter-zeile">
                <input
                  type="checkbox"
                  checked={s.spracheFest}
                  disabled={!!szenario?.language}
                  onChange={(e) => live.spracheSetzen(e.target.checked, s.sprache)}
                /> {t("Sprache festlegen")}
              </label>
              <select value={s.sprache} disabled={!s.spracheFest || !!szenario?.language} onChange={(e) => live.spracheSetzen(true, e.target.value)}>
                {SPRACHEN.map(([id, name]) => <option key={id} value={id}>{name}</option>)}
              </select>
              <span className="live-hinweis">{s.spracheFest ? t("antwortet nur in dieser Sprache") : t("frei: antwortet in deiner Sprache")}</span>
            </div>
            <details className="live-kontext">
              <summary>{t("Kontext / Rolle")}</summary>
              {szenario?.hidden ? (
                <div className="live-hinweis">{t("Der Kontext dieses Szenarios ist verborgen (z. B. ein Fallprofil für Studierende).")}</div>
              ) : (
                <>
                  <textarea rows={6} value={kontext} onChange={(e) => setKontext(e.target.value)} />
                  <div className="live-zeile">
                    <button className="knopf-klein" onClick={() => live.kontextSetzen(kontext)}>{t("Kontext übernehmen")}</button>
                    <button className="knopf-klein" onClick={() => live.kontextSetzen(null)}>{t("Zurücksetzen")}</button>
                    {s.kontext && <span className="live-hinweis">{t("eigener Kontext aktiv")}</span>}
                  </div>
                </>
              )}
            </details>
          </>
        )}
        {s.fehler && <div className="hinweis fehler live-fehler">{s.fehler}</div>}
      </div>

      <div className="live-verlauf" ref={verlauf}>
        {s.wiederhergestellt > 0 && (
          <div className="live-memo">{t("aus dem Gedächtnis:")} {s.wiederhergestellt} {t("Nachrichten")}</div>
        )}
        {s.nachrichten.map((n) => (
          <div key={n.id} className={`live-nachricht ${n.rolle}`}>
            <b>{n.rolle === "du" ? t("Du") : n.rolle === "ki" ? t("KI") : t("Hinweis")}</b>
            {n.sprache && <span className="live-sprache">{n.sprache}</span>}
            <span>{n.text}</span>
            {n.unsicher && <span className="live-unsicher"> {n.unsicher}</span>}
            {n.unterbrochen && <span className="live-unsicher"> … [{t("unterbrochen")}]</span>}
          </div>
        ))}
      </div>

      <div className="live-fuss">
        <div className="live-zeile">
          <button
            className={`knopf ${s.mikrofon ? "gefahr" : "haupt"}`}
            disabled={!verbunden}
            onClick={() => (s.mikrofon ? live.mikrofonAus() : void live.mikrofonAn())}
          >
            {s.mikrofon ? <MicOff size={14} /> : <Mic size={14} />} {s.mikrofon ? t("Mikrofon aus") : t("Sprechen")}
          </button>
          <span className={`live-status ${s.status}`}>{statusText(s)}</span>
        </div>
        <div className="live-zeile">
          <input
            className="live-eingabe"
            value={text}
            disabled={!verbunden}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && senden()}
            placeholder={t("Oder tippen — die Antwort kommt als Sprache")}
          />
          <button className="knopf-klein knopf-symbol" disabled={!verbunden || !text.trim()} onClick={senden} aria-label={t("Senden")}><Send size={14} /></button>
        </div>
        {(s.zeiten.ersterTon !== undefined || s.zeiten.stt !== undefined) && (
          <div className="live-hinweis live-zeiten">
            {s.zeiten.stt !== undefined && <>{t("Erkennung")} {s.zeiten.stt.toFixed(2)} s · </>}
            {s.zeiten.ersterTon !== undefined && <>{t("erster Ton")} {s.zeiten.ersterTon.toFixed(2)} s</>}
            {s.info && <> · Bridge {s.info.version}</>}
          </div>
        )}
      </div>
    </div>
  );
}
