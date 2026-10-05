/**
 * Der Kern — das ist alles, was eine Oberfläche importieren muss.
 *
 *     import { agent, statusLabel, type Snapshot } from "./core/index.ts";
 *
 * Die Aufteilung dahinter:
 *
 *   protocol.ts   ACP als Typen. Deklarativ, keine Laufzeit.
 *   jsonrpc.ts    JSON-RPC 2.0: Rahmen, Zuordnung, Antwortpflicht.
 *   transport.ts  die einzige Stelle, die Tauri kennt.
 *   state.ts      Ansichtsmodell und reine Übergänge darauf.
 *   agent.ts      der Agent als ein Objekt — die Fläche für die Oberfläche.
 *   settings.ts   wie der Agent gestartet wird — und was ausdrücklich nicht gespeichert wird.
 *   preferences.ts  wie die Anwendung sich verhält (Name, Erscheinungsbild).
 *   labels.ts     deutsche Wörter und Töne, damit Komponenten ACP nicht kennen.
 *   preview.ts    was ein Werkzeug tun wird, aus seinen Argumenten gelesen.
 */

export { Agent, agent } from "./agent.ts";
export { dateiname, transcriptMarkdown } from "./export.ts";
export { initErgebnis, initPacks, type InitDatei, type InitErgebnis, type InitPack } from "./einrichten.ts";
export { dokuKarte, dokuTitel, dokuVerweis, type DokuAbschnitt, type DokuSeite, type DokuStatus, type DokuTreffer } from "./doku.ts";
export { locale, onSprache, setSprache, sprache, SPRACHEN, systemSprache, t, type Sprache } from "./i18n.ts";
export { DEFAULT_SPEECH, DEFAULT_TRANSCRIBE, speechModel, sprechbar } from "./speech.ts";
export { bridgeWebSocket, live, type LiveInfo, type LiveNachricht, type LiveSnapshot, type LiveStatus, type LiveSzenario, type LiveVerbindung } from "./realtime.ts";
export { JsonRpcPeer, RpcCode, RpcError } from "./jsonrpc.ts";
export {
  clearStored,
  formatArgs,
  launchArgs,
  mergeConfig,
  parseArgs,
  readStored,
  resolveConfig,
  store,
  type LaunchConfig,
} from "./settings.ts";
export {
  blockToText,
  emptySnapshot,
  MAX_DIAGNOSTICS,
  MAX_TOOL_OUTPUT,
  type AgentMode,
  type GatewayState,
  type MessageItem,
  type MessageRole,
  type NoticeItem,
  type PendingPermission,
  type Snapshot,
  type Status,
  type TerminalView,
  type ToolDiff,
  type ToolItem,
  type TranscriptItem,
} from "./state.ts";
export {
  applyPlan,
  planOf,
  producedFiles,
  visible,
  type Plan,
  type PlannedEdit,
  type PlannedFile,
} from "./preview.ts";
export {
  applyAppearance,
  DEFAULT_PREFERENCES,
  readPreferences,
  resolveAppearance,
  watchAppearance,
  writePreferences,
  type Appearance,
  type PanelLayout,
  type Preferences,
} from "./preferences.ts";
export {
  permissionTone,
  relativeTime,
  roleLabel,
  shortPath,
  statusLabel,
  statusTone,
  toolKindLabel,
  toolKindTone,
  toolStatusLabel,
  toolStatusTone,
  type Tone,
} from "./labels.ts";
export {
  clientCapabilities,
  Method,
  PROTOCOL_VERSION,
  type AgentCapabilities,
  type ContentBlock,
  type PermissionOption,
  type StopReason,
  type ToolKind,
  type ToolStatus,
} from "./protocol.ts";
export {
  tauriTransport,
  type ConfigReport,
  type DoctorCheck,
  type DoctorReport,
  type EnvSpec,
  type BrowserState,
  type ChatHit,
  type ChatMeta,
  type ExportFormat,
  type McpServerEntry,
  type Permissions,
  type DirEntry,
  type GitChange,
  type GitFileDiff,
  type GitState,
  type Rect,
  type DirListing,
  type FileInfo,
  type SheetPreview,
  type TextFile,
  type GatewayModel,
  type GatewayReport,
  type ModelInfo,
  type Readiness,
  type StoredSession,
  type Transport,
} from "./transport.ts";
