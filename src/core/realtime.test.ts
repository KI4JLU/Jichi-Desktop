import { describe, expect, it } from "vitest";
import { bridgeWebSocket } from "./realtime.ts";

describe("bridgeWebSocket: eingefuegte Adresse -> WebSocket der Bridge", () => {
  it("http-Adresse ohne Pfad bekommt /v1/realtime", () => {
    expect(bridgeWebSocket("http://127.0.0.1:8001")).toBe("ws://127.0.0.1:8001/v1/realtime");
  });
  it("ohne Schema wird http angenommen", () => {
    expect(bridgeWebSocket("127.0.0.1:8002")).toBe("ws://127.0.0.1:8002/v1/realtime");
  });
  it("https wird wss", () => {
    expect(bridgeWebSocket("https://bridge.example.org")).toBe("wss://bridge.example.org/v1/realtime");
  });
  it("eine fertige WebSocket-Adresse bleibt, wie sie ist", () => {
    expect(bridgeWebSocket(" ws://localhost:8001/v1/realtime ")).toBe("ws://localhost:8001/v1/realtime");
  });
  it("leer bleibt leer", () => {
    expect(bridgeWebSocket("  ")).toBe("");
  });
});
