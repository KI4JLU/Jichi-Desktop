// AudioWorklet fuer das Live-Gespraech: Mikrofon -> PCM16 mono, 100-ms-Bloecke.
// Als Datei ausgeliefert (nicht als blob:-URL), damit die CSP script-src 'self' genuegt.
class RealtimeCapture extends AudioWorkletProcessor {
  constructor() { super(); this.teile = []; this.laenge = 0; }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    if (!ch) return true;
    const i16 = new Int16Array(ch.length);
    for (let i = 0; i < ch.length; i++) {
      const s = Math.max(-1, Math.min(1, ch[i]));
      i16[i] = s < 0 ? s * 32768 : s * 32767;
    }
    this.teile.push(i16); this.laenge += i16.length;
    if (this.laenge >= 2400) {                      // 100 ms bei 24 kHz
      const out = new Int16Array(this.laenge); let o = 0;
      for (const t of this.teile) { out.set(t, o); o += t.length; }
      this.port.postMessage(out.buffer, [out.buffer]);
      this.teile = []; this.laenge = 0;
    }
    return true;
  }
}
registerProcessor("realtime-capture", RealtimeCapture);
