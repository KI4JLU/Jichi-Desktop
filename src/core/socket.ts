/**
 * Ein WebSocket, der auch eine Kopfzeile mitschicken kann.
 *
 * Der WebSocket des Browsers kann das nicht — der Standard sieht keine
 * Kopfzeilen vor. Die Voice-Bridge verlangt auf ihrem Netzport aber genau eine:
 * `Authorization: Bearer <BRIDGE_API_KEY>`. Ein Versuch aus dem Fenster endet
 * deshalb immer mit `401`, und zwar unabhängig davon, was man einträgt.
 *
 * Darum läuft die Verbindung in der Anwendung über die Rust-Seite. Das löst
 * nicht nur das Problem mit der Kopfzeile, sondern ein zweites, das schwerer
 * wiegt: **der Schlüssel kommt gar nicht erst ins Fenster.** Übergeben wird nur
 * der *Name* seines Eintrags in der geschützten Ablage; gelesen wird er in Rust,
 * im Augenblick des Verbindens. Was die Seite nie sieht, kann sie nicht
 * verlieren — nicht an ein Protokoll, nicht an einen Absturzbericht.
 *
 * Ohne Rust-Seite (Entwicklung im reinen Browser, gegen eine lokale Bridge ohne
 * Schlüssel) wird der WebSocket des Browsers benutzt. Die Schnittstelle ist
 * dieselbe, der aufrufende Code merkt den Unterschied nicht.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Die Teilmenge von `WebSocket`, die das Live-Gespräch wirklich benutzt. */
export interface Socket {
  onopen: (() => void) | null;
  onerror: ((grund: string) => void) | null;
  onclose: (() => void) | null;
  onmessage: ((ereignis: { data: string }) => void) | null;
  /** Offen und beschreibbar. */
  readonly offen: boolean;
  send(text: string): void;
  close(): void;
}

interface Nachricht {
  generation: number;
  line: string;
}

export function imFenster(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Verbindet. Die Rückgabe erfolgt sofort, der Verbindungsaufbau läuft weiter —
 * genau wie beim `WebSocket` des Browsers, damit der Aufrufer seine Rückrufe
 * vorher setzen kann.
 *
 * `secret` ist der **Name** eines Eintrags in der Ablage, nie ein Schlüssel.
 */
export function oeffneSocket(url: string, secret?: string): Socket {
  return imFenster() ? new RustSocket(url, secret) : new BrowserSocket(url);
}

// ── Über die Rust-Seite ──────────────────────────────────────────────────────

class RustSocket implements Socket {
  onopen: (() => void) | null = null;
  onerror: ((grund: string) => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((ereignis: { data: string }) => void) | null = null;

  #generation = 0;
  #zustand: "verbindet" | "offen" | "zu" = "verbindet";
  #loesen: UnlistenFn[] = [];
  /** Vor dem Verbindungsaufbau gesendete Zeilen gehen nicht verloren. */
  #warteschlange: string[] = [];

  constructor(url: string, secret?: string) {
    void this.#start(url, secret);
  }

  get offen(): boolean {
    return this.#zustand === "offen";
  }

  send(text: string): void {
    if (this.#zustand === "zu") return;
    if (this.#zustand === "verbindet") {
      this.#warteschlange.push(text);
      return;
    }
    void invoke("live_send", { generation: this.#generation, line: text }).catch(() => {
      // Die Gegenseite ist weg. Das meldet ohnehin gleich `live-closed`;
      // hier noch einmal zu lärmen hilft niemandem.
    });
  }

  close(): void {
    if (this.#zustand === "zu") return;
    this.#zustand = "zu";
    this.#aufraeumen();
    void invoke("live_close").catch(() => {});
  }

  async #start(url: string, secret?: string): Promise<void> {
    try {
      // Erst zuhören, dann verbinden: sonst gehen die ersten Zeilen verloren.
      this.#loesen = await Promise.all([
        listen<Nachricht>("live-message", (e) => {
          if (e.payload.generation === this.#generation && this.#zustand === "offen") {
            this.onmessage?.({ data: e.payload.line });
          }
        }),
        listen<Nachricht>("live-error", (e) => {
          if (e.payload.generation === this.#generation) this.#scheitern(e.payload.line);
        }),
        listen<number>("live-closed", (e) => {
          if (e.payload === this.#generation) this.#beenden();
        }),
      ]);

      if (this.#zustand === "zu") {
        this.#aufraeumen(); // zwischendurch geschlossen
        return;
      }

      this.#generation = await invoke<number>("live_connect", { url, secret: secret ?? null });
      this.#zustand = "offen";
      this.onopen?.();

      const offen = this.#warteschlange.splice(0);
      for (const zeile of offen) this.send(zeile);
    } catch (ursache) {
      this.#scheitern(ursache instanceof Error ? ursache.message : String(ursache));
    }
  }

  #scheitern(grund: string): void {
    if (this.#zustand === "zu") return;
    this.#zustand = "zu";
    this.#aufraeumen();
    this.onerror?.(grund);
    this.onclose?.();
  }

  #beenden(): void {
    if (this.#zustand === "zu") return;
    this.#zustand = "zu";
    this.#aufraeumen();
    this.onclose?.();
  }

  #aufraeumen(): void {
    for (const loesen of this.#loesen) loesen();
    this.#loesen = [];
    this.#warteschlange = [];
  }
}

// ── Im reinen Browser ────────────────────────────────────────────────────────

class BrowserSocket implements Socket {
  onopen: (() => void) | null = null;
  onerror: ((grund: string) => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((ereignis: { data: string }) => void) | null = null;

  #ws: WebSocket | null = null;

  constructor(url: string) {
    try {
      const ws = new WebSocket(url);
      this.#ws = ws;
      ws.onopen = () => this.onopen?.();
      ws.onerror = () => this.onerror?.(`Keine Verbindung zu ${url}.`);
      ws.onclose = () => this.onclose?.();
      ws.onmessage = (e) => this.onmessage?.({ data: String(e.data) });
    } catch (ursache) {
      // Noch im selben Takt — der Aufrufer hat seine Rückrufe noch nicht
      // gesetzt, also erst beim nächsten melden.
      const grund = ursache instanceof Error ? ursache.message : String(ursache);
      queueMicrotask(() => {
        this.onerror?.(grund);
        this.onclose?.();
      });
    }
  }

  get offen(): boolean {
    return this.#ws?.readyState === WebSocket.OPEN;
  }

  send(text: string): void {
    if (this.offen) this.#ws?.send(text);
  }

  close(): void {
    if (!this.#ws) return;
    this.#ws.onclose = null;
    this.#ws.close();
    this.#ws = null;
  }
}
