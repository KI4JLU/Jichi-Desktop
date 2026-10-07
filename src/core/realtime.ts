/**
 * Live-Gespräch über eine Realtime-Bridge (OpenAI-Realtime-Protokoll).
 *
 * Die Bridge (Projekt `realtime-bridge`) nimmt Sprache entgegen, erkennt sie,
 * lässt ein Modell bzw. jichi antworten und spricht die Antwort. Dieses Modul
 * ist der Client dafür: ein WebSocket, das Mikrofon (AudioWorklet, PCM16
 * 24 kHz), lückenlose Wiedergabe, Live-Untertitel und das Gedächtnis je
 * Szenario. Es ist frei von React — die Seitenleiste liest nur `snapshot()`.
 *
 * Man braucht nur die Adresse der Bridge (`http://127.0.0.1:8001` genügt);
 * Modelle, Stimmen und Szenarien meldet die Bridge selbst.
 */

import { t } from "./i18n.ts";

const RATE = 24000;
const KEY = "jichi-desktop.live";

export interface LiveSzenario {
  id: string;
  title: string;
  description: string;
  engine: string;
  language: string | null;
  hidden: boolean;
  prompt?: string;
}

export interface LiveInfo {
  version: string;
  stt: string;
  llm: string;
  tts: string;
  stimme: string;
  gedaechtnis: "jichi" | "bridge" | null;
}

export interface LiveNachricht {
  id: string;
  rolle: "du" | "ki" | "hinweis";
  text: string;
  /** Noch unsicherer Teil der eigenen Rede (grau). */
  unsicher?: string;
  sprache?: string;
  unterbrochen?: boolean;
}

export type LiveVerbindung = "aus" | "verbinde" | "verbunden" | "fehler";
export type LiveStatus = "bereit" | "hoert" | "du-sprichst" | "pause" | "denkt" | "spricht";

export interface LiveSnapshot {
  url: string;
  verbindung: LiveVerbindung;
  fehler: string | null;
  mikrofon: boolean;
  status: LiveStatus;
  info: LiveInfo | null;
  szenarien: LiveSzenario[];
  szenario: string;
  kontext: string | null;
  spracheFest: boolean;
  sprache: string;
  nachrichten: LiveNachricht[];
  wiederhergestellt: number;
  zeiten: { stt?: number; ersterTon?: number; gesamt?: number };
  /** Lehrenden-Modus: verborgene Szenarien sehen und für alle speichern. */
  lehrende: { verfuegbar: boolean; aktiv: boolean; meldung: string | null; szenario: LiveSzenarioVoll | null };
}

/** Ein Szenario mit vollem Text (nur nach Anmeldung als Lehrende/r). */
export interface LiveSzenarioVoll extends LiveSzenario {
  prompt: string;
  voice_style: string;
  editable: boolean;
  source: string;
}

interface Gespeichert {
  url?: string;
  szenario?: string;
  gespraeche?: Record<string, string>;
  kontexte?: Record<string, string>;
  spracheFest?: boolean;
  sprache?: string;
}

function lesen(): Gespeichert {
  try {
    return JSON.parse(localStorage.getItem(KEY) || "{}") as Gespeichert;
  } catch {
    return {};
  }
}

function schreiben(g: Gespeichert): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(g));
  } catch {
    /* privates Fenster: dann eben ohne Merken */
  }
}

/** "http://host:8001", "host:8001", "ws://host:8001/v1/realtime" → WebSocket-URL. */
export function bridgeWebSocket(eingabe: string): string {
  let u = eingabe.trim();
  if (!u) return "";
  if (!/^[a-z]+:\/\//i.test(u)) u = "http://" + u;
  u = u.replace(/^http:/i, "ws:").replace(/^https:/i, "wss:");
  const url = new URL(u);
  if (url.pathname === "/" || url.pathname === "") url.pathname = "/v1/realtime";
  return url.toString();
}

function neueId(): string {
  return Math.random().toString(16).slice(2) + Date.now().toString(16);
}

function base64(buf: ArrayBuffer): string {
  const u8 = new Uint8Array(buf);
  let s = "";
  for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
  return btoa(s);
}

const norm = (w: string) => w.toLowerCase().replace(/[^\p{L}\p{N}]/gu, "");

class LiveClient {
  private g = lesen();
  private ws: WebSocket | null = null;
  private ctx: AudioContext | null = null;
  private tonPruefung: ReturnType<typeof setTimeout> | null = null;
  private mic: MediaStream | null = null;
  private worklet: AudioWorkletNode | null = null;
  private playHead = 0;
  private quellen: AudioBufferSourceNode[] = [];
  private antwortLaeuft = false;
  private zuhoerer = new Set<() => void>();
  private snap: LiveSnapshot;
  private tStop = 0;
  /** Laufende eigene Äußerung (item_id → Nachricht). */
  private eigene: { item: string; id: string } | null = null;
  /** Laufende KI-Antwort: Text kommt vor dem Ton, gezeigt wird im Takt der Stimme. */
  private ki: { id: string; text: string; gezeigt: number; start: number | null; dauer: number | null; fertig: boolean } | null = null;
  private tick = 0;

  constructor() {
    this.snap = {
      url: this.g.url ?? "",
      verbindung: "aus",
      fehler: null,
      mikrofon: false,
      status: "bereit",
      info: null,
      szenarien: [],
      szenario: this.g.szenario ?? "assistent",
      kontext: null,
      spracheFest: this.g.spracheFest ?? false,
      sprache: this.g.sprache ?? "de",
      nachrichten: [],
      wiederhergestellt: 0,
      zeiten: {},
      lehrende: { verfuegbar: false, aktiv: false, meldung: null, szenario: null },
    };
  }

  // ---- Beobachtung (für useSyncExternalStore) ---------------------------

  snapshot = (): LiveSnapshot => this.snap;
  subscribe = (f: () => void): (() => void) => {
    this.zuhoerer.add(f);
    return () => this.zuhoerer.delete(f);
  };
  private setze(teil: Partial<LiveSnapshot>): void {
    this.snap = { ...this.snap, ...teil };
    for (const f of this.zuhoerer) f();
  }
  private nachricht(id: string, aendern: (n: LiveNachricht) => LiveNachricht): void {
    this.setze({ nachrichten: this.snap.nachrichten.map((n) => (n.id === id ? aendern(n) : n)) });
  }
  private anhaengen(n: LiveNachricht): void {
    this.setze({ nachrichten: [...this.snap.nachrichten, n] });
  }

  // ---- Verbindung -------------------------------------------------------

  async verbinden(url: string): Promise<void> {
    const ziel = bridgeWebSocket(url);
    if (!ziel) {
      this.setze({ fehler: t("Bitte die Adresse der Bridge eintragen.") });
      return;
    }
    this.trennen();
    this.g.url = url.trim();
    schreiben(this.g);
    this.setze({ url: this.g.url, verbindung: "verbinde", fehler: null, nachrichten: [], wiederhergestellt: 0 });
    await new Promise<void>((resolve) => {
      let ws: WebSocket;
      try {
        ws = new WebSocket(ziel);
      } catch (e) {
        this.setze({ verbindung: "fehler", fehler: String(e) });
        resolve();
        return;
      }
      this.ws = ws;
      ws.onopen = () => {
        // session.update erst nach session.created: dann kennt der Client die Szenarien
        this.setze({ verbindung: "verbunden" });
        resolve();
      };
      ws.onerror = () => {
        this.setze({ verbindung: "fehler", fehler: `${t("Keine Verbindung zu")} ${ziel}. ${t("Läuft die Bridge, ist der Tunnel offen?")}` });
        resolve();
      };
      ws.onclose = () => {
        if (this.ws === ws) {
          this.ws = null;
          this.mikrofonAus();
          if (this.snap.verbindung === "verbunden") this.setze({ verbindung: "aus", status: "bereit" });
        }
      };
      ws.onmessage = (e) => this.ereignis(JSON.parse(String(e.data)));
    });
  }

  trennen(): void {
    this.mikrofonAus();
    this.wiedergabeStopp();
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.close();
      this.ws = null;
    }
    this.setze({ verbindung: "aus", status: "bereit", info: null });
  }

  private senden(ev: object): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(ev));
  }

  private gespraech(neu: boolean): string {
    const s = this.snap.szenario;
    this.g.gespraeche ??= {};
    if (neu || !this.g.gespraeche[s]) this.g.gespraeche[s] = "conv_" + neueId();
    schreiben(this.g);
    return this.g.gespraeche[s];
  }

  /** session.update: Mikro an/aus, Sprache, Szenario, Gespräch, Kontext. */
  private sitzung(neu: boolean): void {
    const sz = this.snap.szenarien.find((x) => x.id === this.snap.szenario);
    const kontext = sz?.hidden ? null : (this.g.kontexte?.[this.snap.szenario] || null);
    this.setze({ kontext });
    const fmt = { type: "audio/pcm", rate: RATE };
    this.senden({
      type: "session.update",
      session: {
        type: "realtime",
        output_modalities: ["audio"],
        audio: {
          input: {
            format: fmt,
            transcription: { model: "whisper-1", language: this.snap.spracheFest ? this.snap.sprache : null },
            turn_detection: this.snap.mikrofon
              ? { type: "server_vad", threshold: 0.5, silence_duration_ms: 550, prefix_padding_ms: 300, create_response: true }
              : null,
          },
          output: { format: fmt },
        },
        bridge: { scenario: this.snap.szenario, conversation: this.gespraech(neu), context: kontext },
      },
    });
  }

  // ---- Einstellungen ----------------------------------------------------

  // ---- Lehrende ---------------------------------------------------------

  lehrendeAnmelden(passwort: string): void {
    this.senden({ type: "bridge.teacher.login", password: passwort });
  }

  /** Den (auch verborgenen) Text eines Szenarios für alle speichern. */
  szenarioSpeichern(prompt: string): void {
    const sz = this.snap.lehrende.szenario;
    if (!sz || !this.snap.lehrende.aktiv) return;
    this.senden({ type: "bridge.scenario.save", scenario: { ...sz, prompt } });
    this.setze({ lehrende: { ...this.snap.lehrende, meldung: t("speichere …") } });
  }

  szenarioWaehlen(id: string): void {
    if (this.snap.lehrende.aktiv) {
      this.setze({ lehrende: { ...this.snap.lehrende, szenario: null } });
      this.senden({ type: "bridge.scenario.get", id });
    }
    this.g.szenario = id;
    schreiben(this.g);
    const sz = this.snap.szenarien.find((x) => x.id === id);
    const fest = sz?.language ? { spracheFest: true, sprache: sz.language } : {};
    this.wiedergabeStopp();
    this.setze({ szenario: id, nachrichten: [], wiederhergestellt: 0, ...fest });
    this.sitzung(false);
  }

  neuesGespraech(): void {
    this.wiedergabeStopp();
    this.setze({ nachrichten: [], wiederhergestellt: 0 });
    this.sitzung(true);
  }

  kontextSetzen(text: string | null): void {
    this.g.kontexte ??= {};
    if (text && text.trim()) this.g.kontexte[this.snap.szenario] = text.trim();
    else delete this.g.kontexte[this.snap.szenario];
    schreiben(this.g);
    this.neuesGespraech();
  }

  spracheSetzen(fest: boolean, code: string): void {
    this.g.spracheFest = fest;
    this.g.sprache = code;
    schreiben(this.g);
    this.setze({ spracheFest: fest, sprache: code });
    this.sitzung(false);
  }

  // ---- Audio ------------------------------------------------------------

  private audio(): AudioContext {
    if (!this.ctx || this.ctx.state === "closed") this.ctx = new AudioContext({ sampleRate: RATE });
    if (this.ctx.state === "suspended") void this.ctx.resume();
    if (!this.tick) this.tick = requestAnimationFrame(this.takt);
    return this.ctx;
  }

  /**
   * Ton freigeben — aus jedem Klick und jeder Taste in der Live-Ansicht.
   * WebKit (das Fenster dieser Anwendung unter macOS und Linux) lässt ein
   * AudioContext nur laufen, wenn es in einer Benutzeraktion entsteht oder
   * fortgesetzt wird. Entsteht es erst, wenn die Antwort ankommt, bleibt es
   * stumm „suspended“ — im Chromium der Prüfungen fiel das nicht auf.
   * Dazu der übliche Safari-Griff: ein Sample Stille, im Klick abgespielt.
   */
  tonFreigeben(): void {
    const ctx = this.audio();
    if (ctx.state !== "running") void ctx.resume();
    try {
      const q = ctx.createBufferSource();
      q.buffer = ctx.createBuffer(1, 1, ctx.sampleRate);
      q.connect(ctx.destination);
      q.start(0);
    } catch {
      /* ohne Ton geht der Rest trotzdem */
    }
    if (this.snap.fehler === t("Ton ist blockiert — einmal in die Live-Ansicht klicken.")) this.setze({ fehler: null });
  }

  async mikrofonAn(): Promise<void> {
    if (this.snap.verbindung !== "verbunden") return;
    const ctx = this.audio();
    try {
      this.mic = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true, autoGainControl: true },
      });
    } catch (e) {
      this.setze({ fehler: `${t("Mikrofon nicht verfügbar:")} ${e instanceof Error ? e.message : String(e)}` });
      return;
    }
    await ctx.audioWorklet.addModule("/realtime-capture.js");
    const quelle = ctx.createMediaStreamSource(this.mic);
    this.worklet = new AudioWorkletNode(ctx, "realtime-capture");
    quelle.connect(this.worklet);                 // nicht an die Ausgabe: kein Mithören
    this.worklet.port.onmessage = (e: MessageEvent<ArrayBuffer>) =>
      this.senden({ type: "input_audio_buffer.append", audio: base64(e.data) });
    this.setze({ mikrofon: true, status: "hoert", fehler: null });
    this.sitzung(false);                          // Server-VAD an
  }

  mikrofonAus(): void {
    this.mic?.getTracks().forEach((t) => t.stop());
    this.mic = null;
    this.worklet?.disconnect();
    this.worklet = null;
    if (this.snap.mikrofon) {
      this.setze({ mikrofon: false, status: "bereit" });
      this.sitzung(false);                        // Server-VAD aus
    }
  }

  private abspielen(b64: string): void {
    const ctx = this.audio();
    // Kam die Antwort, bevor jemand geklickt hat, bleibt der Ton gesperrt:
    // dann sagen, statt still zu bleiben. Kurz warten — gleich nach dem
    // Anlegen ist auch ein erlaubtes AudioContext noch einen Moment „suspended“.
    if (ctx.state === "suspended" && !this.tonPruefung) {
      this.tonPruefung = setTimeout(() => {
        this.tonPruefung = null;
        if (this.ctx?.state === "suspended") this.setze({ fehler: t("Ton ist blockiert — einmal in die Live-Ansicht klicken.") });
      }, 300);
    }
    const bin = atob(b64);
    const n = bin.length >> 1;
    const f32 = new Float32Array(n);
    for (let i = 0; i < n; i++) {
      const v = bin.charCodeAt(2 * i) | (bin.charCodeAt(2 * i + 1) << 8);
      f32[i] = (v > 32767 ? v - 65536 : v) / 32768;
    }
    const buf = ctx.createBuffer(1, n, RATE);
    buf.getChannelData(0).set(f32);
    const q = ctx.createBufferSource();
    q.buffer = buf;
    q.connect(ctx.destination);
    const start = Math.max(ctx.currentTime + 0.02, this.playHead);
    q.start(start);
    this.playHead = start + buf.duration;
    if (this.ki && this.ki.start === null) this.ki.start = start;
    this.quellen.push(q);
    q.onended = () => {
      this.quellen = this.quellen.filter((x) => x !== q);
      if (!this.quellen.length && !this.antwortLaeuft) {
        this.kiAbschliessen();
        this.setze({ status: this.snap.mikrofon ? "hoert" : "bereit" });
      }
    };
  }

  private wiedergabeStopp(): void {
    for (const q of this.quellen) {
      try {
        q.stop();
      } catch {
        /* schon zu Ende */
      }
    }
    this.quellen = [];
    this.playHead = 0;
    if (this.ki) this.kiAbschliessen(true);
  }

  /** KI-Text im Takt der Stimme aufdecken (Wörter, nie rückwärts). */
  private takt = (): void => {
    const k = this.ki;
    if (k && k.start !== null && this.ctx) {
      const gespielt = Math.max(0, this.ctx.currentTime - k.start);
      const rate = k.dauer ? k.text.length / k.dauer : 15;
      let n = Math.min(k.text.length, Math.floor(gespielt * rate));
      if (n < k.text.length) {
        const sp = k.text.indexOf(" ", n);
        n = sp < 0 ? k.text.length : sp;
      }
      if (n > k.gezeigt) {
        k.gezeigt = n;
        this.nachricht(k.id, (m) => ({ ...m, text: k.text.slice(0, n) }));
      }
    }
    this.tick = requestAnimationFrame(this.takt);
  };

  private kiAbschliessen(unterbrochen = false): void {
    const k = this.ki;
    if (!k) return;
    this.ki = null;
    this.nachricht(k.id, (m) => ({ ...m, text: unterbrochen ? k.text.slice(0, k.gezeigt) : k.text, unterbrochen }));
  }

  // ---- Eingaben ---------------------------------------------------------

  textSenden(text: string): void {
    const t = text.trim();
    if (!t || this.snap.verbindung !== "verbunden") return;
    this.audio();                                  // Klick = Nutzergeste, Ton darf starten
    if (this.antwortLaeuft || this.quellen.length) {
      this.wiedergabeStopp();
      this.senden({ type: "response.cancel" });
    }
    this.anhaengen({ id: neueId(), rolle: "du", text: t });
    this.tStop = performance.now();
    this.senden({ type: "conversation.item.create", item: { type: "message", role: "user", content: [{ type: "input_text", text: t }] } });
    this.senden({ type: "response.create" });
    this.setze({ status: "denkt" });
  }

  // ---- Ereignisse der Bridge -------------------------------------------

  private ereignis(ev: Record<string, any>): void {
    switch (ev.type) {
      case "bridge.teacher": {
        const aktiv = !!ev.ok;
        this.setze({ lehrende: { ...this.snap.lehrende, aktiv,
          meldung: aktiv ? t("Lehrenden-Modus aktiv") : ev.enabled ? t("Passwort falsch") : t("Lehrenden-Modus ist nicht eingerichtet") } });
        if (aktiv) this.senden({ type: "bridge.scenario.get", id: this.snap.szenario });
        break;
      }
      case "bridge.scenario":
      case "bridge.scenario.saved":
        if (ev.scenario?.id === this.snap.szenario) {
          this.setze({ lehrende: { ...this.snap.lehrende, szenario: ev.scenario as LiveSzenarioVoll,
            meldung: ev.type === "bridge.scenario.saved" ? t("Für alle gespeichert — gilt ab dem nächsten Gespräch") : this.snap.lehrende.meldung } });
        }
        break;
      case "session.created":
      case "session.updated": {
        const b = ev.session?.bridge;
        if (!b) break;
        const szenarien: LiveSzenario[] = b.scenarios ?? this.snap.szenarien;
        const kurz = (m: string | undefined) => (m ?? "–").split("/").pop() ?? "–";
        this.setze({ lehrende: { ...this.snap.lehrende, verfuegbar: !!b.teacher_mode } });
        this.setze({
          info: {
            version: b.version,
            stt: kurz(b.stt?.model),
            llm: kurz(b.llm?.model),
            tts: kurz(b.tts?.model),
            stimme: ev.session?.audio?.output?.voice ?? b.tts?.voice ?? "–",
            gedaechtnis: b.memory ?? null,
          },
          szenarien,
        });
        if (ev.type === "session.created" && szenarien.length && !szenarien.some((x) => x.id === this.snap.szenario)) {
          this.szenarioWaehlen(szenarien[0].id);
        } else if (ev.type === "session.created") {
          this.sitzung(false);                    // jetzt mit bekannter Szenarienliste
        }
        break;
      }
      case "bridge.conversation": {
        const alt: LiveNachricht[] = (ev.messages ?? []).map((m: { role: string; content: string }) => ({
          id: neueId(), rolle: m.role === "user" ? "du" : "ki", text: m.content,
        }));
        this.setze({ nachrichten: [...alt, ...this.snap.nachrichten], wiederhergestellt: alt.length });
        break;
      }
      case "input_audio_buffer.speech_started":
        if (this.antwortLaeuft || this.quellen.length) {
          this.wiedergabeStopp();
          this.senden({ type: "response.cancel" });
        }
        if (!this.eigene || this.eigene.item !== ev.item_id) {
          this.eigene = { item: ev.item_id, id: neueId() };
          this.anhaengen({ id: this.eigene.id, rolle: "du", text: "", unsicher: "" });
        }
        this.setze({ status: "du-sprichst" });
        break;
      case "conversation.item.input_audio_transcription.delta":
        {
          const e = this.eigene;
          if (e && e.item === ev.item_id) {
            this.nachricht(e.id, (m) => ({ ...m, text: m.text + (ev.delta || ""), unsicher: ev.bridge_tentative || "" }));
          }
        }
        break;
      case "input_audio_buffer.speech_stopped":
        this.tStop = performance.now();
        this.setze({ status: "pause" });
        break;
      case "conversation.item.input_audio_transcription.completed": {
        this.setze({ zeiten: { ...this.snap.zeiten, stt: (performance.now() - this.tStop) / 1000 } });
        const fertig = (ev.transcript || "").trim();
        const e = this.eigene;
        if (e && e.item === ev.item_id) {
          const id = e.id;
          this.eigene = null;
          this.nachricht(id, (m) => {
            // Schon Gezeigtes stehen lassen, nur den Rest anhängen
            const alt = m.text.trim().split(/\s+/).filter(Boolean);
            const neu = fertig.split(/\s+/).filter(Boolean);
            const gleich = alt.every((w, i) => i < neu.length && norm(w) === norm(neu[i]));
            const text = !neu.length ? m.text : gleich ? [...alt, ...neu.slice(alt.length)].join(" ") : fertig;
            return { ...m, text, unsicher: "", sprache: ev.bridge_language };
          });
          if (!fertig) this.setze({ nachrichten: this.snap.nachrichten.filter((n) => n.id !== id || n.text) });
        } else if (fertig) {
          this.anhaengen({ id: neueId(), rolle: "du", text: fertig, sprache: ev.bridge_language });
        }
        this.setze({ status: "denkt" });
        break;
      }
      case "response.created":
        this.kiAbschliessen();
        this.antwortLaeuft = true;
        this.ki = { id: neueId(), text: "", gezeigt: 0, start: null, dauer: null, fertig: false };
        this.anhaengen({ id: this.ki.id, rolle: "ki", text: "" });
        break;
      case "response.output_audio_transcript.delta":
        if (this.ki) this.ki.text += ev.delta;     // erst zeigen, wenn gesprochen
        break;
      case "response.output_audio.delta":
        if (this.snap.zeiten.ersterTon === undefined || this.snap.status !== "spricht") {
          this.setze({ status: "spricht", zeiten: { ...this.snap.zeiten, ersterTon: (performance.now() - this.tStop) / 1000 } });
        }
        this.abspielen(ev.delta);
        break;
      case "response.output_audio.done":
        if (this.ki && this.ki.start !== null) this.ki.dauer = this.playHead - this.ki.start;
        break;
      case "response.done": {
        this.antwortLaeuft = false;
        this.setze({ zeiten: { ...this.snap.zeiten, gesamt: (performance.now() - this.tStop) / 1000 } });
        const k = this.ki;
        if (k) {
          if (ev.bridge_language) this.nachricht(k.id, (m) => ({ ...m, sprache: ev.bridge_language }));
          if (ev.response?.status === "cancelled") this.kiAbschliessen(true);
          else if (k.start === null || !this.quellen.length) this.kiAbschliessen();
        }
        if (!this.quellen.length) this.setze({ status: this.snap.mikrofon ? "hoert" : "bereit" });
        break;
      }
      case "error":
        this.anhaengen({ id: neueId(), rolle: "hinweis", text: ev.error?.message ?? t("Fehler") });
        break;
    }
  }
}

export const live = new LiveClient();
