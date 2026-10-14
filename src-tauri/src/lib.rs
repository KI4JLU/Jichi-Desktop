//! jichi-desktop — Prozessverwaltung für den ACP-Agenten.
//!
//! Diese Schicht macht genau eine Sache: sie startet den Agenten als
//! Kindprozess, schiebt Zeilen in dessen stdin und meldet jede Zeile von
//! stdout/stderr als Ereignis an die Oberfläche. Das Protokoll selbst —
//! JSON-RPC 2.0, zeilenweise — wird im Frontend gesprochen, nicht hier. So
//! bleibt die Rust-Seite klein und der Protokollcode an einer Stelle.
//!
//! Drei Dinge, die eine GUI von einer Shell unterscheiden, sind hier gelöst:
//!
//! 1. **PATH.** Eine aus dem Dock gestartete Anwendung erbt kein
//!    Login-Shell-Environment; ihr PATH ist `/usr/bin:/bin:/usr/sbin:/sbin`.
//!    Ein in `/opt/homebrew/bin` oder `~/.local/bin` installiertes Programm ist
//!    dann unsichtbar. Darum wird der PATH des Kindes ergänzt und der Agent
//!    zusätzlich in den üblichen Verzeichnissen gesucht.
//! 2. **Der Schlüssel.** Der Agent liest seinen API-Schlüssel aus einer
//!    Umgebungsvariablen (`apiKeyEnv` in seiner Konfiguration). Diese Anwendung
//!    legt ihn in einer eigenen Datei ab (0600, siehe „Geheimnisse“), liest ihn
//!    nur beim Start des Kindes und setzt dort die Variable. Kein Befehl gibt
//!    ihn an die Oberfläche zurück; er wird nirgends protokolliert.
//! 3. **Plattform.** Unter Linux und macOS läuft der Agent nativ; Windows nennt
//!    das Projekt "not supported by design", dort führt der Weg über WSL2. Die
//!    Fallunterscheidung liegt an dieser einen Stelle.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

mod artefakt;
mod browser;
mod chats;
mod konfig;
mod documents;
mod gateway;
mod git;
mod jichi_doku;
mod mcp_dokumente;
mod projekt;
mod pty;
mod speech;
mod terminal;

// ── Zustand ──────────────────────────────────────────────────────────────────

/// Ein laufendes Kind. Kind und stdin liegen unter einem Schloss, damit es
/// keine Reihenfolge zwischen zwei Schlössern geben kann.
struct Live {
    generation: u64,
    child: Child,
    stdin: Option<ChildStdin>,
}

#[derive(Default)]
struct Acp {
    live: Mutex<Option<Live>>,
    next_gen: AtomicU64,
}

/// Jede Ereignisnutzlast trägt die Generation ihres Kindes.
///
/// Ohne sie geht Folgendes schief: beim Neustart hat der Leser-Thread des alten
/// Kindes noch gepufferte Zeilen und liefert sie nach — die Oberfläche würde
/// Antworten des *toten* Agenten der neuen Sitzung zuschreiben. Das Frontend
/// verwirft jede Zeile, deren Generation nicht die aktuelle ist.
#[derive(Serialize, Clone)]
struct LineEvent {
    generation: u64,
    line: String,
}

#[derive(Serialize, Clone)]
struct ExitEvent {
    generation: u64,
    code: Option<i32>,
}

// ── Pfade und Umgebung ───────────────────────────────────────────────────────

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `~` und `~/…` auflösen. Ein Kindprozess bekommt keine Shell, die das für uns
/// tut, und ein Pfad aus einem Eingabefeld enthält sehr wahrscheinlich eine Tilde.
pub(crate) fn expand_tilde(input: &str) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return home().unwrap_or_else(|| PathBuf::from(trimmed));
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(h) = home() {
            return h.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// Das Trennzeichen im PATH. Unter Windows ist es `;`, überall sonst `:` — mit
/// dem falschen wird der ganze PATH zu einem einzigen unsinnigen Eintrag, und
/// kein Programm ist mehr auffindbar.
const PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };

/// Dateiendungen, unter denen ein Programm ausführbar ist. Unter Windows trägt
/// `jichi` in Wahrheit `jichi.exe`; wer den Namen ohne Endung eintippt, soll
/// trotzdem gefunden werden.
const EXEC_SUFFIXES: &[&str] = if cfg!(windows) {
    &["", ".exe", ".cmd", ".bat"]
} else {
    &[""]
};

/// Verzeichnisse, in denen ein selbst gebautes oder per Paketmanager
/// installiertes Programm liegt, die aber im PATH einer GUI-Anwendung fehlen.
/// Unter Windows gibt es diese Konvention nicht — dort steht alles im PATH.
fn extra_bin_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        return Vec::new();
    }
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ];
    if let Some(h) = home() {
        dirs.push(h.join(".local/bin"));
        dirs.push(h.join("bin"));
        dirs.push(h.join(".cargo/bin"));
    }
    dirs
}

/// Der PATH für das Kind: der eigene, ergänzt um die üblichen Verzeichnisse,
/// ohne Doppelungen und in stabiler Reihenfolge.
fn child_path() -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let push = |p: String, parts: &mut Vec<String>, seen: &mut Vec<String>| {
        if p.is_empty() || seen.contains(&p) {
            return;
        }
        seen.push(p.clone());
        parts.push(p);
    };
    if let Ok(existing) = std::env::var("PATH") {
        for p in existing.split(PATH_SEP) {
            push(p.to_string(), &mut parts, &mut seen);
        }
    }
    for d in extra_bin_dirs() {
        push(d.to_string_lossy().into_owned(), &mut parts, &mut seen);
    }
    parts.join(&PATH_SEP.to_string())
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Darf dieses Programm als Agent gestartet werden?
///
/// Die Oberfläche kann `acp_start`, `doctor` und `probe` mit beliebigem
/// Programm rufen. Gelänge es je, in sie Skript einzuschleusen, wäre das ein
/// Weg, alles Mögliche auszuführen — mit dem API-Schlüssel in der Umgebung.
/// Erlaubt ist darum nur ein Programm, dessen Dateiname mit „jichi“ beginnt,
/// und unter Windows `wsl.exe`, wenn es jichi startet.
fn agent_program_ok(path: &Path, args: &[String]) -> Result<(), String> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if name.starts_with("jichi") {
        return Ok(());
    }
    if cfg!(windows) && (name == "wsl.exe" || name == "wsl") {
        let erstes = args.iter().find(|a| !a.starts_with('-')).map(|a| a.to_ascii_lowercase());
        if erstes.as_deref().is_some_and(|a| a.rsplit(['/', '\\']).next().is_some_and(|n| n.starts_with("jichi"))) {
            return Ok(());
        }
    }
    Err(format!(
        "{} ist nicht jichi — gestartet werden nur Programme namens jichi…",
        path.display()
    ))
}

/// Einen Programmnamen zu einem Pfad auflösen. Enthält die Eingabe einen
/// Schrägstrich, ist sie schon ein Pfad. Sonst wird der ergänzte PATH abgesucht.
fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        let base = expand_tilde(program);
        for suffix in EXEC_SUFFIXES {
            let cand = if suffix.is_empty() {
                base.clone()
            } else {
                PathBuf::from(format!("{}{suffix}", base.display()))
            };
            if is_executable(&cand) {
                return Some(cand);
            }
        }
        return None;
    }
    for dir in child_path().split(PATH_SEP) {
        if dir.is_empty() {
            continue;
        }
        for suffix in EXEC_SUFFIXES {
            let cand = Path::new(dir).join(format!("{program}{suffix}"));
            if is_executable(&cand) {
                return Some(cand);
            }
        }
    }
    // Letzter Ausweg. jichi hat für macOS kein Installationspaket — es wird aus
    // den Quellen gebaut und liegt dann im Projektverzeichnis, in keinem
    // bin-Ordner und in keinem PATH. Genau dort wird jetzt nachgesehen.
    scan_home(program)
}


/// Grenzen der Suche im Heimatverzeichnis. Sie ist ein letzter Ausweg und darf
/// den Start nicht spürbar aufhalten, also ist sie in Tiefe *und* Breite
/// begrenzt und bricht lieber ergebnislos ab, als lange zu graben.
const SCAN_MAX_DEPTH: usize = 4;
const SCAN_MAX_DIRS: usize = 5000;
/// Und eine Uhr darüber. Verzeichnisse zu zählen schätzt den Aufwand schlecht:
/// ein Netzlaufwerk oder ein nachgeladener Cloud-Ordner kostet je Eintrag ein
/// Vielfaches eines lokalen. Gemessen wurde auf diesem Rechner eine kalte Suche
/// von knapp einer Minute — so lange darf ein Programmstart nicht schweigen.
/// Nach Ablauf wird aufgegeben und nach dem Pfad gefragt; das ist ehrlicher als
/// ein Fenster, das sich nicht rührt.
const SCAN_MAX_TIME: std::time::Duration = std::time::Duration::from_secs(4);

/// Verzeichnisse, die die Suche nicht betritt.
///
/// Zwei Gründe, und der zweite wiegt schwerer als der erste.
///
/// **Geschwindigkeit:** `node_modules` und `target` enthalten Zehntausende
/// Dateien und nie ein gesuchtes Programm.
///
/// **Ruhe:** macOS bewacht `Desktop`, `Documents`, `Downloads` und die
/// Medienordner einzeln. Schon ein Blick hinein lässt das System den Benutzer
/// um Erlaubnis fragen — und bei einer Entwicklungsfassung, die bei jedem Bauen
/// eine neue Signatur bekommt, fragt es bei **jedem Start erneut**. Eine Suche,
/// die den Benutzer bei jedem Start vier Dialoge wegklicken lässt, ist keine
/// Hilfe mehr. Wer sein Programm dort liegen hat, trägt den Pfad einmal in den
/// erweiterten Einstellungen ein; alle anderen merken nichts.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            // Vom System bewacht -- ein Blick hinein kostet einen Dialog.
            "Desktop"
                | "Documents"
                | "Downloads"
                | "Music"
                | "Movies"
                | "Pictures"
                | "Photos"
                | "Public"
                | "Library"
                | "Applications"
                | "Mobile Documents"
                // Gross und aussichtslos.
                | "node_modules"
                | "target"
                | "vendor"
                | "Trash"
        )
}

/// Breitensuche nach einer ausführbaren Datei dieses Namens.
///
/// Breite zuerst, weil ein Treffer näher an der Wurzel der wahrscheinlichere
/// ist. Symbolischen Verzeichnisverweisen wird nicht gefolgt — sonst kann die
/// Suche im Kreis laufen.
fn scan_for(root: &Path, program: &str, accept: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut visited = 0usize;
    let start = std::time::Instant::now();

    while let Some((dir, depth)) = queue.pop_front() {
        visited += 1;
        if visited > SCAN_MAX_DIRS || start.elapsed() > SCAN_MAX_TIME {
            return None;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut deeper = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < SCAN_MAX_DEPTH && !skip_dir(&name) {
                    deeper.push(entry.path());
                }
            } else if name == program {
                let path = entry.path();
                if is_executable(&path) && accept(&path) {
                    return Some(path);
                }
            }
        }
        queue.extend(deeper.into_iter().map(|d| (d, depth + 1)));
    }
    None
}

/// Wo diese Anwendung ihre eigenen Notizen ablegt. Kein bewachter Ort, also
/// auch kein Dialog.
fn app_dir() -> Option<PathBuf> {
    // Damit Prüfungen nicht in die echten Notizen des Benutzers schreiben.
    if let Some(override_dir) = std::env::var_os("JICHI_DESKTOP_DIR") {
        let dir = PathBuf::from(override_dir);
        std::fs::create_dir_all(&dir).ok()?;
        return Some(dir);
    }
    let h = home()?;
    let dir = if cfg!(target_os = "macos") {
        h.join("Library/Application Support").join(SECRET_SERVICE)
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(h)
            .join(SECRET_SERVICE)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| h.join(".config"))
            .join(SECRET_SERVICE)
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn found_file() -> Option<PathBuf> {
    Some(app_dir()?.join("gefunden.json"))
}

fn read_found() -> BTreeMap<String, String> {
    found_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Nur ein Programm, das in einem gebauten jichi-Quellbaum liegt, wird aus der
/// Heimatsuche übernommen.
///
/// Die Suche nimmt sonst die erste ausführbare Datei namens `jichi`, die sie
/// findet — auch die aus einem gerade heruntergeladenen fremden Repository,
/// denn git bewahrt das Ausführungsrecht. Dieses Programm bekäme beim nächsten
/// Start den API-Schlüssel in seine Umgebung. Ein echter Bau liegt neben seinen
/// Quellen; genau das wird geprüft, ohne die Datei auszuführen.
fn is_jichi_build(path: &Path) -> bool {
    path.parent().is_some_and(|dir| {
        dir.join("include/jc_version.h").is_file() && dir.join("src/main.c").is_file()
    })
}

/// Gefundene Pfade merken — im Speicher und auf der Platte.
///
/// Die Platte ist der eigentliche Punkt. Ohne sie liefe die Suche bei jedem
/// Programmstart neu, und das ist genau der Moment, in dem das System nach
/// Erlaubnis fragt. Mit ihr wird höchstens **einmal** gesucht: danach steht der
/// Pfad fest und wird nur noch daraufhin geprüft, ob er noch existiert.
/// Verschwindet das Programm, fällt der Eintrag weg und es wird erneut gesucht.
fn scan_home(program: &str) -> Option<PathBuf> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));

    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(program).cloned()) {
        if is_executable(&hit) {
            return Some(hit);
        }
    }

    let mut notiert = read_found();
    if let Some(hit) = notiert.get(program).map(PathBuf::from) {
        if is_executable(&hit) && is_jichi_build(&hit) {
            if let Ok(mut c) = cache.lock() {
                c.insert(program.to_string(), hit.clone());
            }
            return Some(hit);
        }
        notiert.remove(program); // weggezogen: Notiz ist wertlos
    }

    let found = scan_for(&home()?, program, is_jichi_build)?;

    if let Ok(mut c) = cache.lock() {
        c.insert(program.to_string(), found.clone());
    }
    notiert.insert(program.to_string(), found.to_string_lossy().into_owned());
    if let (Some(path), Ok(text)) = (found_file(), serde_json::to_string_pretty(&notiert)) {
        let _ = std::fs::write(path, text);
    }
    Some(found)
}

/// Eine Umgebungsvariable für das Kind.
///
/// Drei Quellen, in dieser Reihenfolge: die geschützte Ablage dieser Anwendung,
/// eine Datei, ein direkter Wert. Für Geheimnisse ist nur die erste gedacht —
/// die beiden anderen bleiben für Sonderfälle und ältere Einstellungen.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct EnvSpec {
    name: String,
    /// Name in der geschützten Ablage. Der Wert wird hier gelesen und nirgends sonst.
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    file: Option<String>,
}

/// Fehlermeldungen nennen nur den Pfad, nie den Inhalt.
fn resolve_env(specs: &[EnvSpec]) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for spec in specs {
        if spec.name.trim().is_empty() {
            continue;
        }
        let value = match (&spec.secret, &spec.value, &spec.file) {
            (Some(account), _, _) if !account.trim().is_empty() => {
                secret_read(account.trim()).ok_or_else(|| {
                    format!("{}: in der Schlüsselablage liegt nichts unter „{account}“", spec.name)
                })?
            }
            (_, _, Some(f)) if !f.trim().is_empty() => {
                let path = expand_tilde(f);
                let raw = std::fs::read_to_string(&path).map_err(|e| {
                    format!("{}: {} ist nicht lesbar ({e})", spec.name, path.display())
                })?;
                let v = raw.trim().to_string();
                if v.is_empty() {
                    return Err(format!("{}: {} ist leer", spec.name, path.display()));
                }
                v
            }
            (_, Some(v), _) if !v.is_empty() => v.clone(),
            _ => continue,
        };
        out.insert(spec.name.clone(), value);
    }
    Ok(out)
}


// ── Geheimnisse ──────────────────────────────────────────────────────────────
//
// Der API-Schlüssel liegt in einer eigenen Datei, die nur dem Benutzer gehört
// (0600), im Datenverzeichnis dieser Anwendung.
//
// Vorher war es der Schlüsselbund des Betriebssystems. Das klingt besser und
// war hier falsch: macOS bindet die Erlaubnis für einen Eintrag an die Signatur
// des fragenden Programms, und eine Entwicklungsfassung bekommt bei jedem Bauen
// eine neue. „Immer erlauben“ kann deshalb gar nicht halten — das System sieht
// jedes Mal ein fremdes Programm und fragt erneut nach dem Anmeldepasswort.
// Ein Schutz, den der Benutzer zehnmal am Tag wegklickt, schützt nichts mehr;
// er erzieht nur dazu, Passwortdialoge blind zu bestätigen.
//
// Was die Datei leistet: andere Benutzerkonten auf demselben Rechner kommen
// nicht heran, und ein versehentliches Mitkopieren beim Teilen eines Ordners
// fällt auf. Was sie nicht leistet: Schutz vor anderen Programmen, die als
// derselbe Benutzer laufen. Das ist derselbe Handel, den `~/.ssh/id_rsa` und
// `~/.aws/credentials` eingehen — und den der Agent selbst schon eingeht, denn
// er bekommt den Schlüssel als Umgebungsvariable.
//
// Unverändert bleibt das Wichtigste: es gibt **keinen Befehl, der den Wert
// zurückgibt**. Geschrieben wird er einmal, gelesen nur hier, beim Start des
// Kindprozesses.

const SECRET_SERVICE: &str = "de.uni-giessen.hrz.jichi-desktop";

fn secret_path(account: &str) -> Option<PathBuf> {
    // Kein Pfadtrenner aus dem Namen, sonst schriebe ein Aufruf irgendwohin.
    if account.is_empty() || account.contains(['/', '\\', '.']) {
        return None;
    }
    let dir = app_dir()?.join("secrets");
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir.join(account))
}

/// Merkt sich eine bewusste Entfernung, damit eine alte Schlüsseldatei den
/// Zugang nicht sofort wiederherstellt. Die alte Datei bleibt unangetastet.
fn secret_disabled_path(account: &str) -> Option<PathBuf> {
    secret_path(account).map(|path| path.with_extension("disabled"))
}

fn legacy_secret_allowed(account: &str) -> bool {
    !secret_disabled_path(account).is_some_and(|path| path.exists())
}

/// Alte Orte, an denen ein Schlüssel schon liegen kann — die in der README
/// genannte Datei und die der JLU. Wird einer gefunden, übernimmt ihn die
/// Anwendung beim ersten Zugriff, damit niemand ihn erneut abtippt.
fn legacy_key_files() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = home() {
        v.push(h.join(".config/jichi/apikey.txt"));
        v.push(h.join(".config/jlu/apikey.txt"));
        v.push(h.join(".jichi-api-key"));
    }
    v
}

fn write_secret(path: &Path, value: &str) -> Result<(), String> {
    std::fs::write(path, value)
        .map_err(|e| format!("Der Schlüssel konnte nicht abgelegt werden: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Die Rechte liessen sich nicht setzen: {e}"))?;
    }
    Ok(())
}

/// Nur innerhalb dieses Moduls. Bewusst **kein** `#[tauri::command]`.
fn secret_read(account: &str) -> Option<String> {
    let path = secret_path(account)?;

    if let Ok(text) = std::fs::read_to_string(&path) {
        let value = text.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }

    // Übernahme aus einer vorhandenen Schlüsseldatei.
    if account == KEY_ENV && legacy_secret_allowed(account) {
        for alt in legacy_key_files() {
            if let Ok(text) = std::fs::read_to_string(&alt) {
                let value = text.trim().to_string();
                if !value.is_empty() {
                    let _ = write_secret(&path, &value);
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Ohne die Datei zu lesen — für die Frage „ist überhaupt einer da?“.
fn secret_noted(account: &str) -> bool {
    let vorhanden = secret_path(account)
        .map(|p| p.metadata().map(|m| m.len() > 0).unwrap_or(false))
        .unwrap_or(false);
    if vorhanden {
        return true;
    }
    account == KEY_ENV
        && legacy_secret_allowed(account)
        && legacy_key_files().iter().any(|p| p.is_file())
}

#[tauri::command]
fn secret_store(account: String, value: String) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Der Schlüssel ist leer.".into());
    }
    let path = secret_path(&account).ok_or("ungültiger Name")?;
    write_secret(&path, value)?;
    if let Some(disabled) = secret_disabled_path(&account) {
        let _ = std::fs::remove_file(disabled);
    }
    Ok(())
}

#[tauri::command]
fn secret_present(account: String) -> bool {
    secret_noted(&account)
}

#[tauri::command]
fn secret_forget(account: String) -> Result<(), String> {
    let path = secret_path(&account).ok_or("ungültiger Name")?;
    if let Some(disabled) = secret_disabled_path(&account) {
        write_secret(&disabled, "disabled")?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        // Nicht vorhanden ist kein Fehler: das Ziel ist erreicht.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Der Schlüssel konnte nicht entfernt werden: {e}")),
    }
}

// ── Konfiguration des Agenten ────────────────────────────────────────────────
//
// Der Agent bringt seine eigene Konfiguration mit (`~/.jichi`): Server, Modelle,
// und — als *Name* einer Umgebungsvariablen, nicht als Wert — woher sein
// Schlüssel kommt. Auf einem frischen Rechner gibt es sie nicht, und ohne sie
// startet er nicht. Genau das muss der erste Start erkennen können.

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    name: String,
    model: String,
    api_base: Option<String>,
    api_key_env: Option<String>,
    roles: Vec<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ConfigReport {
    path: String,
    exists: bool,
    /// Gesetzt, wenn die Datei da ist, aber nicht gelesen werden konnte.
    problem: Option<String>,
    models: Vec<ModelInfo>,
}

fn config_path() -> PathBuf {
    home().unwrap_or_else(|| PathBuf::from(".")).join(".jichi")
}

fn read_config() -> ConfigReport {
    let path = config_path();
    let shown = path.to_string_lossy().into_owned();

    let Ok(text) = std::fs::read_to_string(&path) else {
        return ConfigReport { path: shown, exists: false, problem: None, models: Vec::new() };
    };
    let json = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(j) => j,
        Err(e) => {
            return ConfigReport {
                path: shown,
                exists: true,
                problem: Some(format!("Die Datei ist kein gültiges JSON: {e}")),
                models: Vec::new(),
            }
        }
    };

    let models = json
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .map(|m| ModelInfo {
                    name: m.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    model: m.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    api_base: m.get("apiBase").and_then(|v| v.as_str()).map(str::to_string),
                    api_key_env: m.get("apiKeyEnv").and_then(|v| v.as_str()).map(str::to_string),
                    roles: m
                        .get("roles")
                        .and_then(|v| v.as_array())
                        .map(|r| r.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    ConfigReport { path: shown, exists: true, problem: None, models }
}

/// Der Server der JLU. Die Modellnamen sind der Stand, den das Gateway heute
/// anbietet; ändert er sich, sagt `doctor` es beim ersten Start sofort
/// („the server does not list this model“) — geraten wird hier nichts.
const JLU_API_BASE: &str = "https://api.hrz.uni-giessen.de/v1";
const KEY_ENV: &str = "JICHI_API_KEY";

/// Was die Anwendung zu jichis eigener Einrichtung hinzufügt — nur, was
/// `jichi setup` nicht schreibt und was sie selbst braucht: ein Modell für
/// `embed` (jichis Dokumentation und `codebase_search`), eines für `rerank`,
/// und den Dokumenten-Server dieser Anwendung. Vorhandenes bleibt, wie es ist.
fn jlu_zusaetze(config: &mut serde_json::Value, exe: Option<&Path>) -> Result<(), String> {
    let obj = config.as_object_mut().ok_or("Die Konfiguration ist kein JSON-Objekt.")?;
    let models = obj.entry("models").or_insert_with(|| serde_json::json!([]));
    let list = models.as_array_mut().ok_or("„models“ ist keine Liste.")?;
    let hat_rolle = |list: &Vec<serde_json::Value>, rolle: &str| {
        list.iter().any(|m| m.get("roles").and_then(|r| r.as_array()).is_some_and(|r| r.iter().any(|x| x.as_str() == Some(rolle))))
    };
    for (name, id, rolle) in [("embed", "jlu/qwen3-embedding", "embed"), ("rerank", "jlu/jina-rerank", "rerank")] {
        if !hat_rolle(list, rolle) {
            list.push(serde_json::json!({
                "name": name, "provider": "openai", "model": id, "apiBase": JLU_API_BASE,
                // Der Name der Variablen, nie ihr Wert — wie jichi setup es auch schreibt.
                "apiKeyEnv": KEY_ENV, "roles": [rolle],
            }));
        }
    }
    if let Some(exe) = exe {
        let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!([]));
        let s = servers.as_array_mut().ok_or("„mcpServers“ ist keine Liste.")?;
        if !s.iter().any(|e| e.get("name").and_then(|n| n.as_str()) == Some(DOCS_NAME)) {
            s.push(docs_entry(exe));
        }
    }
    Ok(())
}

// ── Dokumente für den Agenten ────────────────────────────────────────────────
//
// Der Server steckt in diesem Programm (`--mcp-dokumente`). jichi kennt MCP-
// Server nur aus seiner Konfiguration — ACP reicht keine weiter —, also muss
// dort ein Eintrag stehen. Er wird nur auf ausdrücklichen Wunsch geschrieben,
// mit Sicherung der alten Datei.

const DOCS_NAME: &str = "dokumente";

fn docs_entry(exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "name": DOCS_NAME,
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": [mcp_dokumente::FLAG],
        // Lesen ohne Rückfrage; Schreiben fragt wie jedes andere Werkzeug.
        "autoApprove": ["read_document", "list_sheets"]
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DocsStatus {
    /// Ein Eintrag steht in der Konfiguration.
    enabled: bool,
    /// Er zeigt auf ein Programm, das es gibt.
    reachable: bool,
    /// Die Konfiguration ist kein reines JSON (z. B. mit Kommentaren) — dann
    /// schreibt diese Anwendung nicht hinein.
    problem: Option<String>,
}

fn read_config_json_at(path: &Path) -> Result<Option<serde_json::Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| format!("{} ist kein reines JSON (Kommentare?) — bitte den Eintrag von Hand ergänzen.", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn read_config_json() -> Result<Option<serde_json::Value>, String> {
    read_config_json_at(&config_path())
}

fn docs_status_now() -> DocsStatus {
    match read_config_json() {
        Ok(Some(json)) => {
            let entry = json
                .get("mcpServers")
                .and_then(|m| m.as_array())
                .and_then(|a| a.iter().find(|e| e.get("name").and_then(|n| n.as_str()) == Some(DOCS_NAME)));
            let reachable = entry
                .and_then(|e| e.get("command").and_then(|c| c.as_str()))
                .is_some_and(|c| is_executable(Path::new(c)));
            DocsStatus { enabled: entry.is_some(), reachable, problem: None }
        }
        Ok(None) => DocsStatus { enabled: false, reachable: false, problem: None },
        Err(e) => DocsStatus { enabled: false, reachable: false, problem: Some(e) },
    }
}

/// Sicher schreiben: Sicherung, dann Nachbardatei, dann umbenennen; Rechte 0600.
fn write_config_json_at(path: &Path, json: &serde_json::Value) -> Result<(), String> {
    let path = path.to_path_buf();
    if path.exists() {
        std::fs::copy(&path, path.with_extension("bak-desktop"))
            .map_err(|e| format!("Sicherung von {} fehlgeschlagen: {e}", path.display()))?;
    }
    let text = serde_json::to_string_pretty(json).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("desktop-tmp");
    std::fs::write(&tmp, format!("{text}\n")).map_err(|e| format!("{}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// `enable`: Eintrag anlegen oder auf dieses Programm umstellen. Sonst entfernen.
fn docs_set_now(enable: bool) -> Result<DocsStatus, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Eigener Pfad unbekannt: {e}"))?;
    docs_set_in(&config_path(), &exe, enable)?;
    Ok(docs_status_now())
}

fn docs_set_in(config: &Path, exe: &Path, enable: bool) -> Result<(), String> {
    let mut json = read_config_json_at(config)?.ok_or("Es gibt noch keine Konfiguration des Agenten.")?;
    let obj = json.as_object_mut().ok_or("Die Konfiguration ist kein JSON-Objekt.")?;
    let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!([]));
    let list = servers
        .as_array_mut()
        .ok_or("„mcpServers“ ist keine Liste (jichi erwartet eine Liste, siehe docs/MCP.md).")?;
    list.retain(|e| e.get("name").and_then(|n| n.as_str()) != Some(DOCS_NAME));
    if enable {
        list.push(docs_entry(exe));
    }
    write_config_json_at(config, &json)
}

/// Nach einem Umzug des Programms (neue Version, anderer Ort) den Eintrag
/// nachführen — aber nur, wenn der alte Pfad ins Leere zeigt. Zeigt er auf ein
/// anderes, vorhandenes Programm, hat das jemand so gewollt.
fn docs_repair_now() {
    let status = docs_status_now();
    if status.enabled && !status.reachable {
        let _ = docs_set_now(true);
    }
}

/// Legt die Konfiguration des Agenten an. Verweigert, wenn es schon eine gibt —
/// eine fremde Konfiguration zu überschreiben wäre der teuerste denkbare
/// Bedienfehler.
#[tauri::command]
async fn write_config(preset: String, program: String) -> Result<ConfigReport, String> {
    // Im Hintergrund: jichi setup fragt das Gateway, das dauert Sekunden.
    blocking(move || write_config_now(preset, program)).await?
}

fn write_config_now(preset: String, program: String) -> Result<ConfigReport, String> {
    if preset != "jlu" {
        return Err(format!("unbekannte Vorlage „{preset}“"));
    }
    let path = config_path();
    if path.exists() {
        return Err(format!(
            "{} gibt es bereits — sie wird nicht überschrieben.",
            path.display()
        ));
    }
    einrichten_mit_jichi_setup(&program)?;
    // Dazu, was nur diese Anwendung braucht (siehe jlu_zusaetze).
    if let Some(mut json) = read_config_json_at(&path)? {
        jlu_zusaetze(&mut json, std::env::current_exe().ok().as_deref())?;
        write_config_json_at(&path, &json)?;
    }
    // Sie enthält kein Geheimnis, aber sie beschreibt, wo eines herkommt.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(read_config())
}

/// Alex' zweiter Punkt: die globale Konfiguration schreibt jichi selbst —
/// `jichi setup` ohne Rückfragen, mit dem Gateway der JLU (docs/SETUP_WIZARD.md,
/// „Flags“). `--context-length auto` fragt das Gateway nach dem echten Fenster
/// (nur mit Schlüssel). `setup` legt dabei auch Packs und ein Startskript in den
/// Arbeitsordner; beim ersten Start gibt es noch kein Projekt, also läuft es in
/// einem leeren Ordner, der danach verschwindet — die Packs eines Projekts
/// kommen später mit „Projekt einrichten“ (jichi init).
fn einrichten_mit_jichi_setup(program: &str) -> Result<(), String> {
    let (path, vor) = jichi_aufruf(program)?;
    let tmp = std::env::temp_dir().join(format!("jichi-desktop-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let key = secret_read(KEY_ENV);
    let mut args: Vec<String> = vor;
    args.extend(
        [
            "setup", "--non-interactive", "--preset", "developer", "--provider", "openai",
            "--model", "jlu/qwen3-coder-next", "--api-base", JLU_API_BASE, "--key-env", KEY_ENV,
            "--config-target", "global",
        ]
        .map(String::from),
    );
    if key.is_some() {
        args.extend(["--context-length", "auto"].map(String::from));
    }
    let mut env = BTreeMap::new();
    if let Some(k) = key {
        env.insert(KEY_ENV.to_string(), k);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let ergebnis = run_bounded_in(&path, &refs, &env, std::time::Duration::from_secs(90), Some(&tmp));
    let _ = std::fs::remove_dir_all(&tmp);
    let (_stdout, stderr, exit) = ergebnis?;
    if exit != Some(0) || !config_path().exists() {
        let erste = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        return Err(format!("jichi setup hat keine Konfiguration geschrieben ({}). {erste}", exit.map_or("abgebrochen".into(), |c| c.to_string())));
    }
    Ok(())
}

// ── Selbstprüfung des Agenten ────────────────────────────────────────────────

/// Ein Kindprozess mit Frist. Ohne sie hinge der erste Start unbegrenzt, wenn
/// das Gateway nicht antwortet — und das ist der Moment, in dem jemand zum
/// ersten Mal auf „Verbinden“ drückt.
fn run_bounded(
    path: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    limit: std::time::Duration,
) -> Result<(String, String, Option<i32>), String> {
    run_bounded_in(path, args, env, limit, None)
}

fn run_bounded_in(
    path: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    limit: std::time::Duration,
    cwd: Option<&Path>,
) -> Result<(String, String, Option<i32>), String> {
    let mut cmd = Command::new(path);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .args(args)
        .env("PATH", child_path())
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{} ließ sich nicht starten: {e}", path.display()))?;

    // In Fäden lesen: sonst blockiert ein volles Rohr den Prozess, und die
    // Frist unten würde einen Stau messen statt einer Langsamkeit.
    let mut out = child.stdout.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut BufReader::new(s), &mut buf);
            buf
        })
    });
    let mut err = child.stderr.take().map(|s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut BufReader::new(s), &mut buf);
            buf
        })
    });

    let start = std::time::Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code(),
            Ok(None) => {}
            Err(e) => return Err(format!("Warten fehlgeschlagen: {e}")),
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Keine Antwort nach {} Sekunden — ist das Netz erreichbar?",
                limit.as_secs()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let stdout = out.take().and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err.take().and_then(|h| h.join().ok()).unwrap_or_default();
    Ok((stdout, stderr, code))
}

/// `jichi doctor --output json` — der Agent prüft sich selbst.
///
/// Das ist der Grund, warum diese Anwendung nicht selbst gegen das Gateway
/// spricht: Schlüssel, Server, Modellliste und Kontextfenster prüft der Agent
/// bereits, mit seiner eigenen Konfiguration und seinem eigenen Netzstapel. Ein
/// zweiter, hier nachgebauter Prüfweg könnte grün sagen, wo der echte rot ist.
///
/// Der Bericht wird unverändert weitergereicht: `{ok, warn, fail, checks:[…]}`.
fn doctor_now(program: String, env: Option<Vec<EnvSpec>>) -> Result<serde_json::Value, String> {
    let path = which(&program).ok_or_else(|| format!("{program} nicht gefunden"))?;
    agent_program_ok(&path, &[])?;
    let resolved = resolve_env(env.as_deref().unwrap_or(&[]))?;

    let (stdout, stderr, _code) = run_bounded(
        &path,
        &["doctor", "--output", "json"],
        &resolved,
        std::time::Duration::from_secs(45),
    )?;

    serde_json::from_str::<serde_json::Value>(stdout.trim()).map_err(|_| {
        // Ohne Konfiguration schreibt der Agent seine Erklärung auf stderr und
        // gar kein JSON. Diese Erklärung ist für den Benutzer brauchbarer als
        // eine Meldung über ungültiges JSON.
        let hint = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if hint.is_empty() {
            "Der Agent hat keinen lesbaren Bericht geliefert.".to_string()
        } else {
            hint.trim().to_string()
        }
    })
}

// ── Bereitschaft ─────────────────────────────────────────────────────────────

/// Eine einzige Frage: kann losgelegt werden, oder fehlt noch etwas?
///
/// Bewusst ein Aufruf statt vier. Der erste Start soll nicht vier Antworten
/// abwarten und dabei drei Zwischenzustände zeigen, die niemanden interessieren.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Readiness {
    /// Voller Pfad zum Agenten, oder `null`.
    agent: Option<String>,
    version: Option<String>,
    config: ConfigReport,
    key_stored: bool,
    key_env: String,
    /// Wahr, wenn der erste Start nötig ist.
    needs_setup: bool,
}

/// `program` ist das, was der Benutzer eingetragen hat — sonst wäre ein von
/// Hand gewähltes Programm für die Bereitschaft unsichtbar, und der erste Start
/// liesse sich nie abschliessen, wenn die Suche nichts findet.
fn readiness_now(program: Option<String>) -> Readiness {
    let agent = match program.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => which(p).map(|found| found.to_string_lossy().into_owned()),
        None => default_launch_now().resolved,
    };
    let version = agent.as_deref().and_then(|a| probe_now(a.to_string()).ok());
    let config = read_config();
    let key_stored = secret_noted(KEY_ENV);

    Readiness {
        needs_setup: agent.is_none() || !config.exists || config.models.is_empty() || !key_stored,
        agent,
        version,
        config,
        key_stored,
        key_env: KEY_ENV.to_string(),
    }
}


// ── Live-Gespräch: WebSocket zur Voice-Bridge ────────────────────────────────
//
// Warum das hier liegt und nicht im Fenster: die Bridge verlangt auf ihrem
// Netzport `Authorization: Bearer <BRIDGE_API_KEY>`, und ein WebSocket im
// Browser kann **keine** Kopfzeilen setzen -- das sieht der Standard nicht vor.
// Ein Verbindungsversuch aus dem Fenster endet darum immer mit 401.
//
// Der zweite Grund wiegt schwerer: so kommt der Schlüssel gar nicht erst ins
// Fenster. Er wird hier aus der geschützten Ablage gelesen, in die Kopfzeile
// gesetzt und nie an JavaScript gereicht. Was die Seite nie sieht, kann sie
// auch nicht verlieren.

use futures_util::{SinkExt, StreamExt};

struct BridgeConn {
    generation: u64,
    out: tokio::sync::mpsc::UnboundedSender<String>,
    task: tokio::task::AbortHandle,
}

#[derive(Default)]
struct Bridge {
    conn: Mutex<Option<BridgeConn>>,
    next_gen: AtomicU64,
}

fn bridge_stop(live: &Bridge) {
    if let Some(alt) = live.conn.lock().ok().and_then(|mut g| g.take()) {
        alt.task.abort();
    }
}

/// Öffnet die Verbindung. `secret` ist der Name des Eintrags in der Ablage,
/// nicht der Schlüssel selbst -- der verlässt diese Seite nicht.
#[tauri::command]
async fn live_connect(
    app: AppHandle,
    state: State<'_, Bridge>,
    url: String,
    secret: Option<String>,
) -> Result<u64, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    bridge_stop(&state);
    let generation = state.next_gen.fetch_add(1, Ordering::SeqCst) + 1;

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| format!("{url} ist keine gültige Adresse: {e}"))?;

    if let Some(konto) = secret.as_deref().filter(|k| !k.trim().is_empty()) {
        let wert = secret_read(konto.trim())
            .ok_or_else(|| format!("In der Ablage liegt nichts unter „{konto}“."))?;
        let kopf = format!("Bearer {wert}")
            .parse()
            .map_err(|_| "Der Schlüssel enthält Zeichen, die in keine Kopfzeile passen.")?;
        request.headers_mut().insert("Authorization", kopf);
    }

    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("Keine Verbindung zur Bridge: {e}"))?;
    let (mut schreiber, mut leser) = socket.split();

    let (out, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let task_app = app.clone();
    let handle = tokio::spawn(async move {
        use tokio_tungstenite::tungstenite::Message;
        loop {
            tokio::select! {
                // Vom Fenster zur Bridge.
                zu_senden = rx.recv() => match zu_senden {
                    Some(text) => {
                        if schreiber.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                // Von der Bridge zum Fenster.
                empfangen = leser.next() => match empfangen {
                    Some(Ok(Message::Text(text))) => {
                        let _ = task_app.emit(
                            "live-message",
                            LineEvent { generation, line: text.to_string() },
                        );
                    }
                    // Ping/Pong erledigt die Bibliothek; Binärdaten schickt die
                    // Bridge nicht, Audio kommt als base64 im JSON.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        let _ = task_app.emit(
                            "live-error",
                            LineEvent { generation, line: e.to_string() },
                        );
                        break;
                    }
                    None => break,
                },
            }
        }
        let _ = task_app.emit("live-closed", generation);
    });

    *state.conn.lock().map_err(|_| "Zustand gesperrt")? = Some(BridgeConn {
        generation,
        out,
        task: handle.abort_handle(),
    });
    Ok(generation)
}

#[tauri::command]
fn live_send(state: State<Bridge>, generation: u64, line: String) -> Result<(), String> {
    let guard = state.conn.lock().map_err(|_| "Zustand gesperrt")?;
    let conn = guard.as_ref().ok_or("Die Verbindung steht nicht.")?;
    // Nach einem Neustart darf eine alte Oberfläche nicht in die neue Sitzung schreiben.
    if conn.generation != generation {
        return Err("veraltete Verbindung".into());
    }
    conn.out
        .send(line)
        .map_err(|_| "Die Verbindung ist bereits geschlossen.".to_string())
}

#[tauri::command]
fn live_close(state: State<Bridge>) {
    bridge_stop(&state);
}

// ── Startvorschlag ───────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Launch {
    program: String,
    args: Vec<String>,
    /// Wo das Programm gefunden wurde, oder `null`, wenn es nicht auffindbar ist.
    resolved: Option<String>,
    /// Vorgabe für das Arbeitsverzeichnis. ACP verlangt für `session/new` einen
    /// absoluten Pfad, und das Frontend kann keinen erfinden — also liefert ihn
    /// diese Seite mit.
    cwd: String,
    /// Vorschlag für die Schlüsseldatei, falls eine der üblichen existiert.
    env: Vec<EnvHint>,
    hint: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct EnvHint {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

/// Übliche Orte für die Schlüsseldatei. Vorgeschlagen wird nur, was existiert;
/// gelesen wird hier nichts.
fn key_file_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = home() {
        v.push(h.join(".config/jichi/apikey.txt"));
        v.push(h.join(".config/jlu/apikey.txt"));
        v.push(h.join(".jichi-api-key"));
    }
    v
}

/// Der Vorschlag für diese Plattform. Die Oberfläche darf ihn überschreiben —
/// geraten wird hier nur der Normalfall.
fn default_launch_now() -> Launch {
    let windows = cfg!(target_os = "windows");

    let (program, args) = if windows {
        ("wsl.exe".to_string(), vec!["jichi".into(), "--acp".into()])
    } else {
        ("jichi".to_string(), vec!["--acp".into()])
    };

    let resolved = which(&program).map(|p| p.to_string_lossy().into_owned());

    // Die eigene Ablage gewinnt. Eine ältere Schlüsseldatei bleibt ein Weg für
    // die Erstübernahme, bis der Benutzer den Schlüssel ausdrücklich entfernt.
    let env: Vec<EnvHint> = if secret_noted(KEY_ENV) {
        vec![EnvHint { name: KEY_ENV.into(), secret: Some(KEY_ENV.into()), file: None }]
    } else if legacy_secret_allowed(KEY_ENV) {
        key_file_candidates()
            .into_iter()
            .find(|p| p.is_file())
            .map(|p| {
                vec![EnvHint {
                    name: KEY_ENV.into(),
                    secret: None,
                    file: Some(p.to_string_lossy().into_owned()),
                }]
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let hint = if windows {
        "Windows: jichi läuft in WSL2 — nativ wird Windows vom Projekt nicht unterstützt.".into()
    } else if resolved.is_some() {
        "jichi wurde im PATH gefunden.".into()
    } else {
        "jichi ist im PATH nicht auffindbar — hier den vollen Pfad zur Datei eintragen.".into()
    };

    Launch {
        program,
        args,
        resolved,
        cwd: home()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string()),
        env,
        hint,
    }
}

/// Prüft ein Programm, bevor eine Sitzung davon abhängt: aufrufen, erste
/// Ausgabezeile zurückgeben. Das macht das Einstellungsfeld überprüfbar, ohne
/// dass der Benutzer einen Fehlschlag mitten im Gespräch erlebt.
fn probe_now(program: String) -> Result<String, String> {
    let path = which(&program).ok_or_else(|| format!("{program} nicht gefunden"))?;
    agent_program_ok(&path, &[])?;
    let out = Command::new(&path)
        .arg("--version")
        .env("PATH", child_path())
        .output()
        .map_err(|e| format!("{} nicht ausführbar: {e}", path.display()))?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr)
    } else {
        String::from_utf8_lossy(&out.stdout)
    };
    Ok(text.lines().next().unwrap_or("").trim().to_string())
}

// ── Gespeicherte Sitzungen ───────────────────────────────────────────────────

/// Eine Sitzung aus dem Speicher des Agenten. Die ACP-`sessionId` *ist* die
/// Sitzungs-Id des Agenten, darum generationügt der Dateiname, um sie per
/// `session/load` wieder zu öffnen.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StoredSession {
    id: String,
    title: String,
    workspace: Option<String>,
    mode: Option<String>,
    /// Änderungszeit in Sekunden seit Epoche — die Oberfläche sortiert und
    /// formatiert selbst.
    modified: u64,
    turns: usize,
}

fn sessions_now() -> Result<Vec<StoredSession>, String> {
    match home() {
        Some(h) => Ok(sessions_in(&h.join(".jichi.d/sessions"))),
        None => Ok(Vec::new()),
    }
}

#[tauri::command]
fn delete_session(session_id: String) -> Result<(), String> {
    let root = home().ok_or("Das Benutzerverzeichnis wurde nicht gefunden.")?;
    delete_session_in(&root.join(".jichi.d/sessions"), &session_id)?;
    if let Ok(f) = chats_file() {
        chats::forget(&f, &session_id);
    }
    Ok(())
}

/// Die Id wird nie als Pfad verwendet. Nur eine tatsächlich gelesene Sitzung
/// darf gelöscht werden; damit kann selbst eine fremde oder defekte Id nicht
/// aus dem Sitzungsverzeichnis ausbrechen.
fn delete_session_in(dir: &Path, session_id: &str) -> Result<(), String> {
    if session_id.is_empty() {
        return Err("Die Sitzungs-ID fehlt.".into());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("Sitzungen konnten nicht gelesen werden: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json")
            || !entry.file_type().map(|t| t.is_file()).unwrap_or(false)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let id = json
            .get("sessionId")
            .and_then(|v| v.as_str())
            .or_else(|| path.file_stem().and_then(|s| s.to_str()));
        if id == Some(session_id) {
            return std::fs::remove_file(&path)
                .map_err(|e| format!("Chat konnte nicht gelöscht werden: {e}"));
        }
    }
    Err("Dieser Chat ist nicht mehr vorhanden.".into())
}

/// Die Logik getrennt vom Ort, damit sie gegen ein Testverzeichnis läuft.
fn sessions_in(dir: &Path) -> Vec<StoredSession> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Kein Verzeichnis heisst: noch keine Sitzung. Das ist kein Fehler.
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue; // halb geschriebene Datei: überspringen, nicht scheitern
        };

        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        let id = json
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or(stem)
            .unwrap_or_default();
        if id.is_empty() {
            continue;
        }

        let turns = json
            .get("history")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);

        out.push(StoredSession {
            title: json
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("ohne Titel")
                .to_string(),
            workspace: json
                .get("workspaceDirectory")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            mode: json.get("mode").and_then(|v| v.as_str()).map(str::to_string),
            id,
            modified,
            turns,
        });
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

// ── Lebenszyklus des Kindes ──────────────────────────────────────────────────

/// Wie lange ein Agent nach dem Schliessen von stdin Zeit bekommt, von selbst
/// zu gehen — seine Sitzung zu speichern und seine Werkzeuge aufzuräumen —,
/// bevor er beendet wird.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Wartet auf das Ende eines Kindes, ohne ewig zu warten.
fn wait_for(child: &mut Child, limit: std::time::Duration) -> Option<std::process::ExitStatus> {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if start.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => return None,
        }
    }
}

/// Die ganze Prozessgruppe des Agenten beenden, nicht nur ihn selbst.
///
/// Der Agent startet Werkzeuge — `make`, einen Testlauf, einen Server. Mit
/// `kill` auf seine Prozess-Id allein blieben sie als Waisen stehen und
/// schrieben weiter in den Projektordner. Darum läuft er in einer eigenen
/// Gruppe (siehe `acp_start`), und die wird als Ganzes beendet.
#[cfg(unix)]
pub(crate) fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn stop_inner(acp: &Acp) {
    let taken = acp.live.lock().ok().and_then(|mut g| g.take());
    if let Some(mut live) = taken {
        drop(live.stdin.take()); // Pipe schliessen -> der Agent sieht EOF
        if wait_for(&mut live.child, STOP_GRACE).is_some() {
            return;
        }
        #[cfg(unix)]
        {
            signal_group(live.child.id(), "TERM");
            if wait_for(&mut live.child, std::time::Duration::from_millis(500)).is_some() {
                return;
            }
            signal_group(live.child.id(), "KILL");
        }
        let _ = live.child.kill();
        let _ = live.child.wait(); // ernten, sonst bleibt ein Zombie
    }
}

/// Zeilen aus einem Rohr lesen — **als Bytes**, nicht als Text.
///
/// `BufRead::lines()` bricht bei der ersten Zeile ab, die kein gültiges UTF-8
/// ist, und das Rohr läuft voll: der Agent blockiert beim nächsten Schreiben,
/// und mit ihm die ganze Anwendung. Ein Werkzeug, das eine Latin-1-Datei zeigt,
/// genügt dafür. Ungültige Bytes werden hier ersetzt statt gefürchtet.
fn for_each_line(pipe: impl std::io::Read, mut each: impl FnMut(String)) {
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                each(String::from_utf8_lossy(&buf).into_owned());
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Nach dem Ende von stdout: den Rückgabewert holen, ohne das Schloss zu halten.
///
/// Wer beim Warten auf das Kind das Schloss hält, sperrt jeden anderen Befehl
/// aus — auch `acp_stop` und das Schliessen des Fensters. Darum wird nur kurz
/// gesperrt und nachgesehen, und zwischen zwei Blicken ist das Schloss frei.
fn reap_after_eof(app: &AppHandle, generation: u64) -> Option<i32> {
    let start = std::time::Instant::now();
    loop {
        {
            let state = app.state::<Acp>();
            let mut guard = state.live.lock().ok()?;
            let live = guard.as_mut().filter(|l| l.generation == generation)?;
            match live.child.try_wait() {
                Ok(Some(status)) => {
                    // Geerntet: der Eintrag gehört niemandem mehr.
                    let code = status.code();
                    *guard = None;
                    return code;
                }
                Ok(None) => {}
                Err(_) => return None,
            }
            // stdout zu, Prozess lebt noch: er hat sein Rohr geschlossen, antwortet
            // also nie mehr. Nach einer Frist wird er beendet statt vergessen.
            if start.elapsed() > STOP_GRACE {
                drop(guard);
                stop_inner(state.inner());
                return None;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn acp_start_now(
    app: AppHandle,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<EnvSpec>>,
) -> Result<u64, String> {
    // Erst alles prüfen, was scheitern kann, dann den alten Prozess beenden:
    // ein Tippfehler im Pfad soll nicht die laufende Sitzung mitnehmen.
    let path = which(&program).ok_or_else(|| {
        format!("{program} nicht gefunden — vollen Pfad in den Einstellungen eintragen")
    })?;
    agent_program_ok(&path, &args)?;
    let extra_env = resolve_env(env.as_deref().unwrap_or(&[]))?;

    let work_dir = match cwd.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => {
            let p = expand_tilde(d);
            if !p.is_dir() {
                return Err(format!("{} ist kein Verzeichnis", p.display()));
            }
            Some(p)
        }
        None => None,
    };

    let state = app.state::<Acp>();
    stop_inner(state.inner());

    let generation = state.next_gen.fetch_add(1, Ordering::SeqCst) + 1;

    let mut cmd = Command::new(&path);
    cmd.args(&args)
        .env("PATH", child_path())
        .envs(&extra_env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &work_dir {
        cmd.current_dir(dir);
    }
    // Eine eigene Prozessgruppe, damit `stop_inner` auch die Werkzeuge des
    // Agenten erreicht — und damit ein Strg-C im Terminal von `tauri dev` nicht
    // am Agenten vorbei ins Leere geht.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{} konnte nicht gestartet werden: {e}", path.display()))?;

    let stdout = child.stdout.take().ok_or("kein stdout")?;
    let stderr = child.stderr.take().ok_or("kein stderr")?;
    let stdin = child.stdin.take();

    *state.live.lock().map_err(|_| "Zustand gesperrt")? = Some(Live { generation, child, stdin });

    // stdout: jede Zeile ist eine JSON-RPC-Nachricht.
    let out_app = app.clone();
    std::thread::spawn(move || {
        for_each_line(stdout, |line| {
            if !line.trim().is_empty() {
                let _ = out_app.emit("acp-line", LineEvent { generation, line });
            }
        });
        // EOF: der Agent ist fertig. Den Rückgabewert nur ernten, wenn wirklich
        // noch unser Kind im Zustand steht — sonst wäre das ein Fremdprozess.
        let code = reap_after_eof(&out_app, generation);
        let _ = out_app.emit("acp-exit", ExitEvent { generation, code });
    });

    // stderr: Diagnose des Agenten, nicht Protokoll. Getrennt halten, sonst
    // landet eine Warnung im JSON-Parser und sieht aus wie ein Protokollfehler.
    // Bis zum Ende lesen, auch nach ungültigen Bytes: ein volles stderr-Rohr
    // hält den Agenten genauso an wie ein volles stdout.
    let err_app = app.clone();
    std::thread::spawn(move || {
        for_each_line(stderr, |line| {
            let _ = err_app.emit("acp-stderr", LineEvent { generation, line });
        });
    });

    Ok(generation)
}

// ── Was der Schlüssel erreicht ───────────────────────────────────────────────

/// Der Server, an den der Agent seinen Schlüssel schickt: der erste Eintrag
/// seiner Konfiguration, der ihn aus `JICHI_API_KEY` liest.
fn gateway_base() -> String {
    read_config()
        .models
        .iter()
        .find(|m| m.api_key_env.as_deref() == Some(KEY_ENV) && m.api_base.is_some())
        .and_then(|m| m.api_base.clone())
        .unwrap_or_else(|| JLU_API_BASE.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GatewayReport {
    base: String,
    models: Vec<gateway::GatewayModel>,
}

fn gateway_models_now() -> Result<GatewayReport, String> {
    let key = secret_read(KEY_ENV).ok_or("Es ist kein API-Schlüssel hinterlegt.")?;
    let base = gateway_base();
    let models = gateway::fetch(&base, &key)?;
    Ok(GatewayReport { base, models })
}

// ── Dateien für die Vorschau ─────────────────────────────────────────────────

/// Obergrenze für eine Datei in der Vorschau. Ein Diff über Megabytes liest
/// niemand, und er friert die Anzeige ein.
const PREVIEW_MAX: u64 = 2 * 1024 * 1024;

/// Den heutigen Inhalt einer Datei lesen — damit eine Berechtigungsfrage zeigen
/// kann, was sich ändern *wird*, statt nur den neuen Text.
///
/// Nur innerhalb des Projektordners. `None` heisst: die Datei gibt es noch
/// nicht (sie wird neu angelegt).
fn read_workspace_file_in(cwd: &Path, path: &str) -> Result<Option<String>, String> {
    let root = cwd
        .canonicalize()
        .map_err(|_| format!("{} ist nicht lesbar", cwd.display()))?;
    let wanted = {
        let p = expand_tilde(path);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let file = match wanted.canonicalize() {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Neue Datei: ihr Ordner muss dennoch im Projekt liegen.
            let parent = wanted.parent().and_then(|p| p.canonicalize().ok());
            return match parent {
                Some(p) if p.starts_with(&root) => Ok(None),
                _ => Err("Die Datei liegt ausserhalb des Projekts.".into()),
            };
        }
        Err(e) => return Err(format!("{}: {e}", wanted.display())),
    };
    if !file.starts_with(&root) {
        return Err("Die Datei liegt ausserhalb des Projekts.".into());
    }
    let meta = file.metadata().map_err(|e| format!("{}: {e}", file.display()))?;
    if !meta.is_file() {
        return Err(format!("{} ist keine Datei", file.display()));
    }
    if meta.len() > PREVIEW_MAX {
        return Err("Die Datei ist für eine Vorschau zu gross.".into());
    }
    let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Obergrenze für eine angehängte Textdatei. Größeres gehört nicht in einen
/// Prompt, sondern in den Projektordner, wo der Agent es selbst liest.
const ATTACH_MAX: u64 = 256 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    name: String,
    path: String,
    text: String,
}

/// Eine vom Benutzer im Systemdialog gewählte Textdatei für den nächsten Zug.
/// Binärdateien werden abgelehnt: als „Text“ wären sie für das Modell Müll.
fn read_attachment_now(path: &str) -> Result<Attachment, String> {
    let p = expand_tilde(path);
    let meta = p.metadata().map_err(|e| format!("{}: {e}", p.display()))?;
    if !meta.is_file() {
        return Err(format!("{} ist keine Datei", p.display()));
    }
    // PDF, Word, Excel …: als Text, den das Modell lesen kann.
    if documents::is_document(&p) {
        let text = documents::read(&p, &documents::Auswahl { max_chars: Some(60_000), ..Default::default() })?;
        return Ok(Attachment {
            name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: p.to_string_lossy().into_owned(),
            text,
        });
    }
    if meta.len() > ATTACH_MAX {
        return Err(format!("{} ist größer als 256 KB.", p.display()));
    }
    let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    if bytes.contains(&0) {
        return Err(format!("{} ist keine Textdatei.", p.display()));
    }
    Ok(Attachment {
        name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        path: p.to_string_lossy().into_owned(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

// ── Dateien, die der Agent erzeugt hat ───────────────────────────────────────
//
// Unter einer fertigen Werkzeugkarte steht die Datei, die dabei entstand —
// öffnen, im Finder zeigen, anderswo speichern. Jeder Befehl prüft selbst,
// dass die Datei im Projekt liegt: die Oberfläche nennt nur einen Pfad.

/// Endungen, die „Öffnen“ an das Standardprogramm gibt. Alles andere (ein
/// Skript, eine .app, eine .command-Datei) würde beim Öffnen *ausgeführt* —
/// dafür gibt es nur „Im Finder zeigen“.
const OPENABLE: &[&str] = &[
    "pdf", "docx", "doc", "xlsx", "xls", "ods", "odt", "pptx", "csv", "tsv", "txt", "md", "markdown",
    "json", "xml", "yaml", "yml", "toml", "log", "png", "jpg", "jpeg", "gif", "webp", "svg", "rtf",
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileInfo {
    path: String,
    name: String,
    size: u64,
    modified: u64,
    openable: bool,
}

fn project_file(cwd: &str, path: &str) -> Result<PathBuf, String> {
    let root = expand_tilde(cwd).canonicalize().map_err(|_| format!("{cwd} ist nicht lesbar"))?;
    let p = expand_tilde(path);
    let full = if p.is_absolute() { p } else { root.join(p) };
    let file = full.canonicalize().map_err(|_| format!("{path} gibt es nicht (mehr)."))?;
    if !file.starts_with(&root) {
        return Err("Die Datei liegt ausserhalb des Projekts.".into());
    }
    if !file.is_file() {
        return Err(format!("{path} ist keine Datei."));
    }
    Ok(file)
}

fn openable(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| OPENABLE.contains(&e.to_ascii_lowercase().as_str()))
}

fn file_info_now(cwd: &str, path: &str) -> Result<FileInfo, String> {
    let file = project_file(cwd, path)?;
    let meta = file.metadata().map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(FileInfo {
        name: file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        size: meta.len(),
        modified: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0),
        openable: openable(&file),
        path: file.to_string_lossy().into_owned(),
    })
}

/// Eine Zeile an den Agenten. Der Zeilenumbruch gehört zum Rahmen des
/// Protokolls und wird hier angehängt, nicht vom Aufrufer.
#[tauri::command]
fn acp_send(state: State<Acp>, line: String) -> Result<(), String> {
    let mut guard = state.live.lock().map_err(|_| "Zustand gesperrt")?;
    let live = guard.as_mut().ok_or("der Agent läuft nicht")?;
    let pipe = live.stdin.as_mut().ok_or("stdin ist geschlossen")?;
    pipe.write_all(line.as_bytes())
        .and_then(|_| pipe.write_all(b"\n"))
        .and_then(|_| pipe.flush())
        .map_err(|e| format!("Schreiben fehlgeschlagen: {e}"))
}

// ── Befehle ──────────────────────────────────────────────────────────────────
//
// Alles, was auf einen Kindprozess oder die Platte wartet, läuft **neben** dem
// Hauptfaden. Ein gewöhnlicher Tauri-Befehl läuft auf ihm — und solange er
// wartet, lässt sich das Fenster nicht verschieben und keine Antwort des
// Agenten zustellen. `doctor` allein darf bis zu 45 Sekunden dauern.

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("Hintergrundarbeit abgebrochen: {e}"))
}

#[tauri::command]
async fn default_launch() -> Result<Launch, String> {
    blocking(default_launch_now).await
}

#[tauri::command]
async fn probe(program: String) -> Result<String, String> {
    blocking(move || probe_now(program)).await?
}

#[tauri::command]
async fn sessions() -> Result<Vec<StoredSession>, String> {
    blocking(sessions_now).await?
}

#[tauri::command]
async fn readiness(program: Option<String>) -> Result<Readiness, String> {
    blocking(move || readiness_now(program)).await
}

#[tauri::command]
async fn doctor(program: String, env: Option<Vec<EnvSpec>>) -> Result<serde_json::Value, String> {
    blocking(move || doctor_now(program, env)).await?
}

#[tauri::command]
async fn acp_start(
    app: AppHandle,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<Vec<EnvSpec>>,
) -> Result<u64, String> {
    blocking(move || acp_start_now(app, program, args, cwd, env)).await?
}

#[tauri::command]
async fn acp_stop(app: AppHandle) -> Result<(), String> {
    blocking(move || stop_inner(app.state::<Acp>().inner())).await
}

// ── Seitenleiste ─────────────────────────────────────────────────────────────

#[tauri::command]
async fn list_dir(cwd: String, path: String) -> Result<projekt::Liste, String> {
    blocking(move || projekt::list_dir(&cwd, &path)).await?
}

#[tauri::command]
async fn read_text(cwd: String, path: String) -> Result<projekt::Text, String> {
    blocking(move || projekt::read_text(&cwd, &path)).await?
}

#[tauri::command]
async fn write_text(cwd: String, path: String, text: String, expected_modified: Option<u64>) -> Result<u64, String> {
    blocking(move || projekt::write_text(&cwd, &path, &text, expected_modified.map(u128::from)).map(|m| m as u64)).await?
}

#[tauri::command]
async fn read_sheets(cwd: String, path: String) -> Result<Vec<projekt::Blatt>, String> {
    blocking(move || projekt::read_sheets(&cwd, &path)).await?
}

/// Ein Dokument (Word, PowerPoint, OpenDocument) als Markdown für die Vorschau.
#[tauri::command]
async fn read_document_preview(cwd: String, path: String) -> Result<String, String> {
    blocking(move || {
        let root = projekt::root_of(&cwd)?;
        let file = projekt::inside(&root, &path)?;
        documents::read(&file, &documents::Auswahl::default())
    })
    .await?
}

/// Den Projektordner für das `asset:`-Protokoll freigeben — damit Bilder, PDFs
/// und HTML aus dem Projekt in der Seitenleiste angezeigt werden können. Nur
/// dieser Ordner, und nur lesend.
#[tauri::command]
fn allow_project_assets(app: AppHandle, cwd: String) -> Result<(), String> {
    let root = projekt::root_of(&cwd)?;
    app.asset_protocol_scope()
        .allow_directory(&root, true)
        .map_err(|e| format!("Freigabe fehlgeschlagen: {e}"))
}

// ── Chats: Namen, Anheften, Suche, Export ────────────────────────────────────

fn chats_file() -> Result<PathBuf, String> {
    Ok(chats::meta_file(&app_dir().ok_or("Anwendungsordner fehlt")?))
}

#[tauri::command]
async fn chats_meta() -> Result<std::collections::BTreeMap<String, chats::Meta>, String> {
    blocking(|| Ok(chats::read_meta(&chats_file()?))).await?
}

/// `rename`: ob `title` gilt (auch `null` = eigenen Namen entfernen).
#[tauri::command]
async fn chat_meta_set(id: String, rename: bool, title: Option<String>, pinned: Option<bool>) -> Result<std::collections::BTreeMap<String, chats::Meta>, String> {
    blocking(move || chats::set_meta(&chats_file()?, &id, rename.then_some(title), pinned)).await?
}

#[tauri::command]
async fn search_chats(query: String) -> Result<Vec<chats::Treffer>, String> {
    blocking(move || {
        let dir = home().ok_or("Heimatverzeichnis fehlt")?.join(".jichi.d/sessions");
        Ok(chats::search(&dir, &query))
    })
    .await?
}

/// Einen Chat an einen gewählten Ort schreiben: Markdown, Word oder PDF.
#[tauri::command]
async fn export_chat(dest: String, format: String, title: String, markdown: String) -> Result<(), String> {
    blocking(move || {
        let file = expand_tilde(&dest);
        match format.as_str() {
            "md" => std::fs::write(&file, &markdown).map_err(|e| format!("{}: {e}", file.display())),
            "docx" => documents::write_docx(&file, Some(&title), &markdown),
            "pdf" => documents::write_pdf(&file, Some(&title), &markdown).map(|_| ()),
            other => Err(format!("unbekanntes Format {other}")),
        }
    })
    .await?
}

/// Ein Bild für den Anhang (Ziehen und Ablegen): Base64 und Typ, höchstens 5 MB.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bild {
    mime_type: String,
    data: String,
}

#[tauri::command]
async fn read_image(path: String) -> Result<Bild, String> {
    blocking(move || {
        use base64::Engine as _;
        let p = expand_tilde(&path);
        let mime = match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => return Err("Nur PNG, JPEG, GIF oder WebP.".into()),
        };
        let meta = p.metadata().map_err(|e| format!("{}: {e}", p.display()))?;
        if meta.len() > 5 * 1024 * 1024 {
            return Err("Das Bild ist größer als 5 MB.".into());
        }
        let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(Bild { mime_type: mime.into(), data: base64::engine::general_purpose::STANDARD.encode(bytes) })
    })
    .await?
}

// ── jichis Einstellungen: Erlaubnisse und MCP-Server ─────────────────────────

fn konfig_lesen() -> Result<serde_json::Value, String> {
    read_config_json()?.ok_or_else(|| "Es gibt noch keine Konfiguration des Agenten.".into())
}

fn konfig_aendern(f: impl FnOnce(&mut serde_json::Value) -> Result<(), String>) -> Result<(), String> {
    let mut j = konfig_lesen()?;
    f(&mut j)?;
    write_config_json_at(&config_path(), &j)
}

#[tauri::command]
async fn permissions_get() -> Result<konfig::Erlaubnisse, String> {
    blocking(|| Ok(konfig::erlaubnisse(&konfig_lesen()?))).await?
}

#[tauri::command]
async fn permissions_set(allow: Vec<String>, deny: Vec<String>) -> Result<konfig::Erlaubnisse, String> {
    blocking(move || {
        konfig_aendern(|j| konfig::setze_erlaubnisse(j, &allow, &deny))?;
        Ok(konfig::erlaubnisse(&konfig_lesen()?))
    })
    .await?
}

#[tauri::command]
async fn mcp_list() -> Result<Vec<konfig::McpServer>, String> {
    blocking(|| Ok(konfig::server(&konfig_lesen()?, DOCS_NAME))).await?
}

#[tauri::command]
async fn mcp_toggle(name: String, enable: bool) -> Result<Vec<konfig::McpServer>, String> {
    blocking(move || {
        konfig_aendern(|j| konfig::schalte_server(j, &name, enable))?;
        Ok(konfig::server(&konfig_lesen()?, DOCS_NAME))
    })
    .await?
}

#[tauri::command]
async fn mcp_add(name: String, command: String, args: Vec<String>) -> Result<Vec<konfig::McpServer>, String> {
    blocking(move || {
        konfig_aendern(|j| konfig::neuer_server(j, &name, &command, &args))?;
        Ok(konfig::server(&konfig_lesen()?, DOCS_NAME))
    })
    .await?
}

#[tauri::command]
async fn mcp_remove(name: String) -> Result<Vec<konfig::McpServer>, String> {
    blocking(move || {
        konfig_aendern(|j| konfig::entferne_server(j, &name))?;
        Ok(konfig::server(&konfig_lesen()?, DOCS_NAME))
    })
    .await?
}

/// `jichi mcp`: verbindet sich mit allen Servern und listet ihre Werkzeuge —
/// ohne Modell, ohne Netz zum Gateway.
#[tauri::command]
async fn mcp_test(program: String, env: Option<Vec<EnvSpec>>) -> Result<String, String> {
    blocking(move || {
        let path = which(&program).ok_or_else(|| format!("{program} nicht gefunden"))?;
        agent_program_ok(&path, &[])?;
        let resolved = resolve_env(env.as_deref().unwrap_or(&[]))?;
        let (out, err, _) = run_bounded(&path, &["mcp"], &resolved, std::time::Duration::from_secs(40))?;
        Ok(format!("{}{}", out.trim_end(), if err.trim().is_empty() { String::new() } else { format!("\n{}", err.trim_end()) }))
    })
    .await?
}

#[tauri::command]
fn artifact_put(app: AppHandle, id: String, lang: String, title: String, code: String) -> Result<(), String> {
    app.state::<artefakt::Artefakte>().put(&id, &lang, &title, &code)
}

#[tauri::command]
fn browser_open(app: AppHandle, id: String, url: String, x: f64, y: f64, w: f64, h: f64) -> Result<String, String> {
    browser::open(&app, &id, &url, x, y, w, h)
}

#[tauri::command]
fn browser_bounds(app: AppHandle, id: String, x: f64, y: f64, w: f64, h: f64, visible: bool) -> Result<(), String> {
    browser::bounds(&app, &id, x, y, w, h, visible)
}

#[tauri::command]
fn browser_navigate(app: AppHandle, id: String, url: String) -> Result<String, String> {
    browser::navigate(&app, &id, &url)
}

#[tauri::command]
fn browser_go(app: AppHandle, id: String, wohin: String) -> Result<(), String> {
    browser::go(&app, &id, &wohin)
}

#[tauri::command]
fn browser_close(app: AppHandle, id: String) {
    browser::close(&app, &id)
}

#[tauri::command]
fn pty_open(app: AppHandle, cwd: String, cols: u16, rows: u16) -> Result<u32, String> {
    app.state::<pty::Ptys>().open(&app, &cwd, cols, rows, &child_path())
}

#[tauri::command]
fn pty_write(app: AppHandle, id: u32, data: String) -> Result<(), String> {
    app.state::<pty::Ptys>().write(id, &data)
}

#[tauri::command]
fn pty_resize(app: AppHandle, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    app.state::<pty::Ptys>().resize(id, cols, rows)
}

#[tauri::command]
async fn pty_close(app: AppHandle, id: u32) -> Result<(), String> {
    blocking(move || app.state::<pty::Ptys>().close(id)).await
}

#[tauri::command]
async fn git_changes(cwd: String) -> Result<git::Stand, String> {
    blocking(move || git::stand(&cwd)).await?
}

#[tauri::command]
async fn git_file_diff(cwd: String, path: String) -> Result<git::Vergleich, String> {
    blocking(move || git::vergleich(&cwd, &path)).await?
}

#[tauri::command]
async fn file_info(cwd: String, path: String) -> Result<FileInfo, String> {
    blocking(move || file_info_now(&cwd, &path)).await?
}

#[tauri::command]
fn open_file(app: AppHandle, cwd: String, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let file = project_file(&cwd, &path)?;
    if !openable(&file) {
        return Err("Diese Art Datei wird nicht geöffnet, nur im Finder gezeigt.".into());
    }
    app.opener()
        .open_path(file.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("Konnte nicht geöffnet werden: {e}"))
}

#[tauri::command]
fn reveal_file(app: AppHandle, cwd: String, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let file = project_file(&cwd, &path)?;
    app.opener().reveal_item_in_dir(&file).map_err(|e| format!("Konnte nicht gezeigt werden: {e}"))
}

/// Eine Kopie dorthin, wo der Benutzer sie im Speichern-Dialog haben will.
#[tauri::command]
async fn save_file_copy(cwd: String, path: String, dest: String) -> Result<(), String> {
    blocking(move || {
        let file = project_file(&cwd, &path)?;
        let dest = expand_tilde(&dest);
        if dest == file {
            return Ok(());
        }
        std::fs::copy(&file, &dest).map(|_| ()).map_err(|e| format!("{}: {e}", dest.display()))
    })
    .await?
}

#[tauri::command]
async fn documents_status() -> Result<DocsStatus, String> {
    blocking(|| {
        docs_repair_now();
        docs_status_now()
    })
    .await
}

#[tauri::command]
async fn documents_set(enable: bool) -> Result<DocsStatus, String> {
    blocking(move || docs_set_now(enable)).await?
}

// ── Einrichten über jichi (init) ─────────────────────────────────────────────
// Alex' zweiter Punkt: die Einrichtung baut auf jichi auf. Die Anwendung ruft
// `jichi init` und liest seine Ausgabe; die Packs und was sie schreiben sind
// jichis (docs/SCAFFOLDING.md). Hinaus gehen nur Packnamen, keine freien Argumente.

fn pack_name_ok(n: &str) -> bool {
    // Beginnt mit Buchstabe oder Ziffer: sonst wäre `--force` ein „Pack“.
    n.len() <= 40
        && n.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// So wird jichi für einen Unterbefehl gerufen — unter Windows über wsl.exe.
fn jichi_aufruf(program: &str) -> Result<(PathBuf, Vec<String>), String> {
    let path = which(program).ok_or_else(|| format!("{program} nicht gefunden"))?;
    let stamm = path.file_stem().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let vor: Vec<String> = if stamm == "wsl" { vec!["jichi".into()] } else { Vec::new() };
    agent_program_ok(&path, &vor)?;
    Ok((path, vor))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JichiAusgabe {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
}

fn jichi_init_now(program: &str, cwd: Option<&str>, packs: &[String], probe: bool, liste: bool) -> Result<JichiAusgabe, String> {
    let (path, vor) = jichi_aufruf(program)?;
    let mut args: Vec<String> = vor;
    args.push("init".into());
    let dir = match cwd {
        Some(c) => Some(projekt::fuer_programme(&projekt::root_of(c)?)),
        None => None,
    };
    if liste {
        args.push("--list".into());
    } else {
        if dir.is_none() {
            return Err("Kein Projekt geöffnet.".into());
        }
        if packs.is_empty() || packs.len() > 12 || !packs.iter().all(|p| pack_name_ok(p)) {
            return Err("Ungültige Auswahl der Packs.".into());
        }
        args.extend(packs.iter().cloned());
        if probe {
            args.push("--dry-run".into());
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (stdout, stderr, exit) =
        run_bounded_in(&path, &refs, &BTreeMap::new(), std::time::Duration::from_secs(60), dir.as_deref())?;
    Ok(JichiAusgabe { exit, stdout, stderr })
}

#[tauri::command]
async fn jichi_init_liste(program: String) -> Result<JichiAusgabe, String> {
    blocking(move || jichi_init_now(&program, None, &[], false, true)).await?
}

/// `probe`: nur zeigen, was geschähe (`--dry-run`).
#[tauri::command]
async fn jichi_init(program: String, cwd: String, packs: Vec<String>, probe: bool) -> Result<JichiAusgabe, String> {
    blocking(move || jichi_init_now(&program, Some(&cwd), &packs, probe, false)).await?
}

// ── jichis Dokumentation ─────────────────────────────────────────────────────

/// Ein vom Benutzer genannter Ort der Dokumentation (wenn sie nicht neben dem
/// Programm liegt — etwa unter Windows, wo jichi in WSL läuft).
fn doku_pfad_datei() -> Option<PathBuf> {
    Some(app_dir()?.join("doku.json"))
}

fn doku_pfad_eingestellt() -> Option<PathBuf> {
    let text = std::fs::read_to_string(doku_pfad_datei()?).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get("pfad").and_then(|p| p.as_str()).map(expand_tilde)
}

fn doku_ort(program: &str) -> Option<jichi_doku::DokuOrt> {
    let prog = which(program);
    jichi_doku::finden(prog.as_deref(), doku_pfad_eingestellt().as_deref())
}

fn doku_root(program: &str) -> Result<PathBuf, String> {
    doku_ort(program)
        .map(|o| PathBuf::from(o.root))
        .ok_or_else(|| "Die Dokumentation von jichi wurde nicht gefunden.".to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DokuStatus {
    ort: Option<jichi_doku::DokuOrt>,
    seiten: usize,
    /// Ob jichi die Dokumentation als Quelle für `search_docs` hat.
    quelle: jichi_doku::Quellenstand,
    /// Die Konfiguration ist nicht lesbar/schreibbar (Kommentare …).
    problem: Option<String>,
}

fn doku_status_now(program: &str) -> DokuStatus {
    let ort = doku_ort(program);
    let seiten = ort.as_ref().map(|o| jichi_doku::liste(Path::new(&o.root)).len()).unwrap_or(0);
    let root = ort.as_ref().map(|o| o.root.as_str());
    let (quelle, problem) = match read_config_json() {
        Ok(Some(json)) => (jichi_doku::quellenstand(&json, root), None),
        Ok(None) => (jichi_doku::quellenstand(&serde_json::json!({}), root), None),
        Err(e) => (jichi_doku::quellenstand(&serde_json::json!({}), root), Some(e)),
    };
    DokuStatus { ort, seiten, quelle, problem }
}

#[tauri::command]
async fn jichi_doku_status(program: String) -> Result<DokuStatus, String> {
    blocking(move || doku_status_now(&program)).await
}

/// Den Ort nennen (oder mit `None` vergessen). Nur ein Ordner, der wie jichis
/// `docs/` aussieht, wird angenommen.
#[tauri::command]
async fn jichi_doku_pfad(program: String, pfad: Option<String>) -> Result<DokuStatus, String> {
    blocking(move || {
        let datei = doku_pfad_datei().ok_or("Anwendungsordner fehlt.")?;
        match pfad.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
            Some(p) => {
                let dir = expand_tilde(&p);
                // Der Ordner des Quellbaums darf es auch sein: dann ist docs/ gemeint.
                let dir = if jichi_doku::ist_jichi_doku(&dir) { dir } else { dir.join("docs") };
                if !jichi_doku::ist_jichi_doku(&dir) {
                    return Err(format!("{p}: das ist nicht die Dokumentation von jichi (docs/ mit README.md, VOCABULARY.md …)."));
                }
                std::fs::write(&datei, serde_json::json!({ "pfad": dir.to_string_lossy() }).to_string())
                    .map_err(|e| e.to_string())?;
            }
            None => {
                let _ = std::fs::remove_file(&datei);
            }
        }
        Ok(doku_status_now(&program))
    })
    .await?
}

#[tauri::command]
async fn jichi_doku_liste(program: String) -> Result<Vec<String>, String> {
    blocking(move || Ok(jichi_doku::liste(&doku_root(&program)?))).await?
}

#[tauri::command]
async fn jichi_doku_lesen(program: String, seite: String) -> Result<String, String> {
    blocking(move || jichi_doku::lesen(&doku_root(&program)?, &seite)).await?
}

#[tauri::command]
async fn jichi_doku_suchen(program: String, anfrage: String) -> Result<Vec<jichi_doku::Treffer>, String> {
    blocking(move || Ok(jichi_doku::suchen(&doku_root(&program)?, &anfrage, 60))).await?
}

/// jichi die Dokumentation als Nachschlagewerk geben (Eintrag `docs` der
/// Konfiguration, mit Sicherung) — oder wieder nehmen.
#[tauri::command]
async fn jichi_doku_fuer_agent(program: String, an: bool) -> Result<DokuStatus, String> {
    blocking(move || {
        let path = config_path();
        let mut json = read_config_json_at(&path)?.ok_or("Es gibt noch keine Konfiguration des Agenten.")?;
        let root = if an { Some(doku_root(&program)?) } else { None };
        jichi_doku::quelle_setzen(&mut json, root.as_ref().map(|r| r.to_string_lossy()).as_deref())?;
        write_config_json_at(&path, &json)?;
        Ok(doku_status_now(&program))
    })
    .await?
}

#[tauri::command]
async fn read_attachment(path: String) -> Result<Attachment, String> {
    blocking(move || read_attachment_now(&path)).await?
}

#[tauri::command]
async fn gateway_models() -> Result<GatewayReport, String> {
    blocking(gateway_models_now).await?
}

/// Diktieren: der Körper ist die Aufnahme selbst (rohe Bytes, kein JSON), das
/// Format steht im Kopf `x-mime`, das Modell in `x-model`, die Sprache in
/// `x-language` (leer = das Modell erkennt sie).
#[tauri::command]
async fn speech_transcribe(request: tauri::ipc::Request<'_>) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(audio) = request.body() else {
        return Err("Die Aufnahme kam nicht als Bytes an.".into());
    };
    let audio = audio.clone();
    let head = |k: &str| request.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let (mime, model, lang) = (head("x-mime"), head("x-model"), head("x-language"));
    blocking(move || {
        let key = secret_read(KEY_ENV).ok_or("Es ist kein API-Schlüssel hinterlegt.")?;
        let model = if model.is_empty() { speech::DEFAULT_TRANSCRIBE.to_string() } else { model };
        speech::transcribe(&gateway_base(), &key, &model, &audio, &mime, Some(lang.as_str()).filter(|l| !l.is_empty()))
    })
    .await?
}

/// Vorlesen: Text hinein, mp3 als rohe Bytes zurück (kommt als ArrayBuffer an).
#[tauri::command]
async fn speech_speak(text: String, model: Option<String>, voice: Option<String>) -> Result<tauri::ipc::Response, String> {
    let bytes = blocking(move || {
        let key = secret_read(KEY_ENV).ok_or("Es ist kein API-Schlüssel hinterlegt.")?;
        let model = model.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| speech::DEFAULT_SPEECH.to_string());
        speech::speak(&gateway_base(), &key, &model, &text, voice.as_deref().unwrap_or("alloy"))
    })
    .await??;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
async fn read_workspace_file(cwd: String, path: String) -> Result<Option<String>, String> {
    blocking(move || read_workspace_file_in(&expand_tilde(&cwd), &path)).await?
}

#[tauri::command]
fn term_create(
    app: AppHandle,
    command: String,
    args: Option<Vec<String>>,
    cwd: Option<String>,
    output_byte_limit: Option<usize>,
) -> Result<String, String> {
    app.state::<terminal::Terminals>().create(
        &app,
        &command,
        &args.unwrap_or_default(),
        cwd.as_deref(),
        output_byte_limit,
        &child_path(),
    )
}

#[tauri::command]
fn term_output(app: AppHandle, terminal_id: String) -> Result<terminal::Output, String> {
    app.state::<terminal::Terminals>().output(&terminal_id)
}

#[tauri::command]
async fn term_wait(app: AppHandle, terminal_id: String) -> Result<terminal::ExitStatus, String> {
    blocking(move || app.state::<terminal::Terminals>().wait(&terminal_id)).await?
}

#[tauri::command]
fn term_kill(app: AppHandle, terminal_id: String) -> Result<(), String> {
    app.state::<terminal::Terminals>().kill(&terminal_id)
}

#[tauri::command]
fn term_release(app: AppHandle, terminal_id: String) -> Result<(), String> {
    app.state::<terminal::Terminals>().release(&terminal_id)
}

#[tauri::command]
fn acp_running(state: State<Acp>) -> bool {
    state
        .live
        .lock()
        .map(|mut g| match g.as_mut() {
            Some(live) => matches!(live.child.try_wait(), Ok(None)),
            None => false,
        })
        .unwrap_or(false)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Als Dokumenten-Server gestartet (von jichi, nicht vom Benutzer): kein
    // Fenster, nur JSON-RPC über stdin/stdout.
    if std::env::args().any(|a| a == mcp_dokumente::FLAG) {
        mcp_dokumente::serve();
        return;
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Acp::default())
        .manage(Bridge::default())
        .manage(terminal::Terminals::default())
        .manage(pty::Ptys::default())
        .manage(artefakt::Artefakte::default())
        .register_uri_scheme_protocol("artefakt", |ctx, req| {
            ctx.app_handle().state::<artefakt::Artefakte>().antwort(&req)
        })
        .setup(|app| {
            browser::remember(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            default_launch,
            probe,
            sessions,
            delete_session,
            readiness,
            doctor,
            write_config,
            secret_store,
            secret_present,
            secret_forget,
            acp_start,
            acp_send,
            acp_stop,
            acp_running,
            gateway_models,
            jichi_doku_status,
            jichi_init_liste,
            jichi_init,
            jichi_doku_pfad,
            jichi_doku_liste,
            jichi_doku_lesen,
            jichi_doku_suchen,
            jichi_doku_fuer_agent,
            speech_transcribe,
            speech_speak,
            read_attachment,
            documents_status,
            documents_set,
            file_info,
            list_dir,
            git_changes,
            git_file_diff,
            artifact_put,
            chats_meta,
            chat_meta_set,
            search_chats,
            export_chat,
            read_image,
            permissions_get,
            permissions_set,
            mcp_list,
            mcp_toggle,
            mcp_add,
            mcp_remove,
            mcp_test,
            browser_open,
            browser_bounds,
            browser_navigate,
            browser_go,
            browser_close,
            pty_open,
            pty_write,
            pty_resize,
            pty_close,
            read_text,
            write_text,
            read_sheets,
            read_document_preview,
            allow_project_assets,
            open_file,
            reveal_file,
            save_file_copy,
            read_workspace_file,
            term_create,
            term_output,
            term_wait,
            term_kill,
            term_release,
                    live_connect,
            live_send,
            live_close
        ])
        // Ohne das überlebt der Agent das Fenster und bleibt als Waise im
        // Prozessbaum stehen — beim Entwickeln nach zehn Neustarts zehn Agenten.
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                window.app_handle().state::<terminal::Terminals>().kill_all();
                window.app_handle().state::<pty::Ptys>().close_all();
                stop_inner(window.app_handle().state::<Acp>().inner());
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ── Prüfungen ────────────────────────────────────────────────────────────────
//
// Nur die Entscheidungen, die ohne Fenster und ohne Kindprozess zu treffen sind.
// Das ist genau die Hälfte, in der die Fehler sitzen: Pfade, Umgebung, und das
// Lesen fremder Dateien, die auch halb geschrieben sein können.

#[cfg(test)]
mod tests {
    use super::*;

    /// Alle Prüfungen arbeiten in einem eigenen Verzeichnis. Ohne das schrieben
    /// sie in die Notizen des Benutzers — und eine Prüfung, die den geprüften
    /// Rechner verändert, ist keine Prüfung mehr.
    fn eigene_ablage() {
        static EINMAL: OnceLock<()> = OnceLock::new();
        EINMAL.get_or_init(|| {
            let dir = std::env::temp_dir().join("jichi-desktop-test-ablage");
            std::fs::remove_dir_all(&dir).ok();
            std::env::set_var("JICHI_DESKTOP_DIR", &dir);
        });
    }

    #[test]
    fn tilde_wird_aufgeloest() {
        let h = home().expect("HOME ist gesetzt");
        assert_eq!(expand_tilde("~"), h);
        assert_eq!(expand_tilde("~/Schreibtisch"), h.join("Schreibtisch"));
        // Nur am Anfang, und nur mit Trenner danach.
        assert_eq!(expand_tilde("/tmp/~"), PathBuf::from("/tmp/~"));
        assert_eq!(expand_tilde("~unbekannt/x"), PathBuf::from("~unbekannt/x"));
        // Leerraum aus einem Eingabefeld gehört nicht in einen Pfad.
        assert_eq!(expand_tilde("  /tmp  "), PathBuf::from("/tmp"));
    }

    #[test]
    fn pfad_des_kindes_ist_doppelungsfrei() {
        let path = child_path();
        let parts: Vec<&str> = path.split(PATH_SEP).collect();
        let mut sorted = parts.clone();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "doppelte Einträge in {path}");
        if !cfg!(windows) {
            assert!(parts.contains(&"/usr/bin"), "/usr/bin fehlt in {path}");
        }
        // Mit dem falschen Trennzeichen bliebe der ganze PATH ein einziger Eintrag.
        assert!(parts.len() > 1, "PATH wurde nicht zerlegt: {path}");
    }

    /// Die ganze erste Einrichtung gegen ein echtes jichi, in einem leeren HOME.
    /// Nur von Hand und allein (setzt HOME):
    /// `JICHI_TEST_PROGRAMM=…/jichi/jichi cargo test tests::erste_einrichtung_echt -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn erste_einrichtung_echt() {
        let home = std::env::temp_dir().join(format!("jichi-desktop-test-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("app")).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("JICHI_DESKTOP_DIR", home.join("app"));
        let prog = std::env::var("JICHI_TEST_PROGRAMM").unwrap();
        let bericht = write_config_now("jlu".into(), prog).expect("eingerichtet");
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join(".jichi")).unwrap()).unwrap();
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
        assert!(json["_comment"].as_str().unwrap().contains("jichi setup"), "von jichi geschrieben");
        assert_eq!(json["models"][0]["model"], "jlu/qwen3-coder-next");
        assert_eq!(json["models"][0]["apiKeyEnv"], KEY_ENV);
        assert!(json["models"].as_array().unwrap().iter().any(|m| m["roles"][0] == "embed"));
        assert!(bericht.exists);
        // Keine Packs im HOME: setup lief in einem leeren Ordner, der weg ist.
        assert!(!home.join("AGENTS.md").exists() && !home.join("run.sh").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn zusaetze_nur_was_jichi_setup_nicht_schreibt() {
        // So schreibt jichi setup: ein Modell, keine Rollen embed/rerank.
        let mut c = serde_json::json!({"models":[{"name":"chat","model":"jlu/qwen3-coder-next","roles":["chat","edit","apply"]}],"snapshots":true});
        jlu_zusaetze(&mut c, Some(Path::new("/app/jichi-desktop"))).unwrap();
        let ids: Vec<_> = c["models"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap()).collect();
        assert_eq!(ids, ["jlu/qwen3-coder-next", "jlu/qwen3-embedding", "jlu/jina-rerank"]);
        assert_eq!(c["models"][1]["apiKeyEnv"], KEY_ENV, "nie ein Schlüssel, nur sein Name");
        assert!(c["models"][1].get("apiKey").is_none());
        assert_eq!(c["mcpServers"][0]["name"], DOCS_NAME);
        assert_eq!(c["snapshots"], true, "jichis Einstellungen bleiben");
        // Zweimal ist wie einmal.
        let vorher = c.clone();
        jlu_zusaetze(&mut c, Some(Path::new("/app/jichi-desktop"))).unwrap();
        assert_eq!(c, vorher);
    }

    #[test]
    fn nur_packnamen_gehen_an_jichi_init() {
        assert!(pack_name_ok("web-ts") && pack_name_ok("default") && pack_name_ok("c-cli"));
        for boese in ["", "--force", "-x", "a b", "../x", "Default", "x;rm", &"a".repeat(41)] {
            assert!(!pack_name_ok(boese), "{boese}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn which_findet_im_pfad_und_als_pfad() {
        eigene_ablage();
        // Welches `sh` zuerst kommt, hängt vom PATH ab: macOS findet /bin/sh,
        // Ubuntu (zusammengelegtes /usr) zuerst /usr/bin/sh — dieselbe Datei.
        let sh = which("sh").expect("sh im PATH");
        assert_eq!(sh.canonicalize().unwrap(), PathBuf::from("/bin/sh").canonicalize().unwrap(), "{}", sh.display());
        assert_eq!(which("/bin/sh"), Some(PathBuf::from("/bin/sh")));
        assert_eq!(which("gibt-es-ganz-sicher-nicht-42"), None);
        // Ein Verzeichnis ist nicht ausführbar, auch wenn es das Bit gesetzt hat.
        assert_eq!(which("/tmp"), None);
    }

    /// Unter Windows startet die Anwendung `wsl.exe` — genau das muss sie finden,
    /// mit und ohne Endung.
    #[cfg(windows)]
    #[test]
    fn which_findet_wsl_und_cmd_unter_windows() {
        eigene_ablage();
        let klein = |p: Option<PathBuf>| p.map(|p| p.to_string_lossy().to_lowercase());
        assert!(klein(which("cmd")).is_some_and(|p| p.ends_with("\\cmd.exe")), "{:?}", which("cmd"));
        assert!(klein(which("wsl.exe")).is_some_and(|p| p.ends_with("\\wsl.exe")), "{:?}", which("wsl.exe"));
        assert_eq!(which("gibt-es-ganz-sicher-nicht-42"), None);
    }

    #[test]
    fn umgebung_aus_datei_wird_getrimmt() {
        let dir = std::env::temp_dir().join("jichi-desktop-test-env");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key.txt");
        std::fs::write(&file, "  sk-geheim\n\n").unwrap();

        let env = resolve_env(&[EnvSpec {
            name: "JICHI_API_KEY".into(), secret: None,
            value: None,
            file: Some(file.to_string_lossy().into_owned()),
        }])
        .unwrap();
        assert_eq!(env.get("JICHI_API_KEY").map(String::as_str), Some("sk-geheim"));

        // Eine Datei gewinnt gegen einen mitgeschickten Wert: sie ist die frischere Quelle.
        let env = resolve_env(&[EnvSpec {
            name: "JICHI_API_KEY".into(), secret: None,
            value: Some("veraltet".into()),
            file: Some(file.to_string_lossy().into_owned()),
        }])
        .unwrap();
        assert_eq!(env.get("JICHI_API_KEY").map(String::as_str), Some("sk-geheim"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fehlende_oder_leere_schluesseldatei_nennt_den_pfad_nicht_den_inhalt() {
        let err = resolve_env(&[EnvSpec {
            name: "JICHI_API_KEY".into(), secret: None,
            value: None,
            file: Some("/gibt/es/nicht.txt".into()),
        }])
        .unwrap_err();
        assert!(err.contains("/gibt/es/nicht.txt"), "{err}");

        let dir = std::env::temp_dir().join("jichi-desktop-test-leer");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("leer.txt");
        std::fs::write(&file, "   \n").unwrap();
        let err = resolve_env(&[EnvSpec {
            name: "JICHI_API_KEY".into(), secret: None,
            value: None,
            file: Some(file.to_string_lossy().into_owned()),
        }])
        .unwrap_err();
        assert!(err.contains("leer"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leere_namen_und_werte_werden_uebergangen() {
        let env = resolve_env(&[
            EnvSpec { name: "  ".into(), secret: None, value: Some("x".into()), file: None },
            EnvSpec { name: "OHNE_WERT".into(), secret: None, value: None, file: None },
            EnvSpec { name: "MIT_WERT".into(), secret: None, value: Some("ja".into()), file: None },
        ])
        .unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env.get("MIT_WERT").map(String::as_str), Some("ja"));
    }

    #[test]
    fn sitzungen_werden_gelesen_und_kaputte_uebersprungen() {
        let dir = std::env::temp_dir().join("jichi-desktop-test-sessions");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("aaa.json"),
            r#"{"sessionId":"aaa","title":"erstes","workspaceDirectory":"/tmp","mode":"chat","history":[{"role":"user"},{"role":"assistant"}]}"#,
        )
        .unwrap();
        // Halb geschrieben — der Agent speichert, während wir lesen.
        std::fs::write(dir.join("bbb.json"), "{\"sessionId\":\"bbb\",").unwrap();
        // Kein JSON, gehört nicht hierher.
        std::fs::write(dir.join("notiz.txt"), "egal").unwrap();
        // Ohne Titel: die Anzeige braucht trotzdem ein Wort.
        std::fs::write(dir.join("ccc.json"), r#"{"sessionId":"ccc","history":[]}"#).unwrap();

        let found = sessions_in(&dir);
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "gefunden: {ids:?}");
        assert!(ids.contains(&"aaa") && ids.contains(&"ccc"), "{ids:?}");

        let aaa = found.iter().find(|s| s.id == "aaa").unwrap();
        assert_eq!(aaa.title, "erstes");
        assert_eq!(aaa.turns, 2);
        assert_eq!(aaa.workspace.as_deref(), Some("/tmp"));
        assert_eq!(found.iter().find(|s| s.id == "ccc").unwrap().title, "ohne Titel");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nur_die_gewaehlte_sitzung_wird_geloescht() {
        let dir = std::env::temp_dir().join(format!("jichi-desktop-delete-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("eins.json"), r#"{"sessionId":"eins","title":"A"}"#).unwrap();
        std::fs::write(dir.join("zwei.json"), r#"{"sessionId":"zwei","title":"B"}"#).unwrap();
        let ausserhalb = dir
            .parent()
            .unwrap()
            .join(format!("jichi-delete-guard-{}", std::process::id()));
        std::fs::write(&ausserhalb, "behalten").unwrap();

        assert!(delete_session_in(&dir, "../jichi-delete-guard").is_err());
        assert!(ausserhalb.exists());
        delete_session_in(&dir, "eins").unwrap();
        assert!(!dir.join("eins.json").exists());
        assert!(dir.join("zwei.json").exists());

        std::fs::remove_file(ausserhalb).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Nur unter Unix: die Suche unterscheidet ein Programm von einer Datei am
    /// Ausführbar-Bit, und das gibt es unter Windows nicht. Dort startet die
    /// Anwendung jichi ohnehin über `wsl.exe`, nicht über diese Suche.
    #[cfg(unix)]
    #[test]
    fn heimatsuche_findet_das_gebaute_programm() {
        eigene_ablage();
        let root = std::env::temp_dir().join("jichi-desktop-test-scan");
        std::fs::remove_dir_all(&root).ok();

        let gesucht = root.join("projekte/jichi");
        std::fs::create_dir_all(&gesucht).unwrap();
        let ziel = gesucht.join("faux");
        std::fs::write(&ziel, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ziel, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Gleichnamige Dateien an Orten, die übersprungen werden müssen.
        for tarnung in ["node_modules", ".versteckt", "Library"] {
            let dir = root.join(tarnung);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("faux"), "egal").unwrap();
        }
        // Und eine nicht ausführbare Datei desselben Namens.
        std::fs::write(root.join("faux"), "kein Programm").unwrap();

        assert_eq!(scan_for(&root, "faux", |_| true), Some(ziel.clone()));
        assert_eq!(scan_for(&root, "gibt-es-nicht", |_| true), None);

        // Zu tief: jenseits der Grenze wird nicht mehr gesucht.
        let tief = root.join("a/b/c/d/e/f");
        std::fs::create_dir_all(&tief).unwrap();
        std::fs::write(tief.join("tief"), "x").unwrap();
        assert_eq!(scan_for(&root, "tief", |_| true), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn heimatsuche_nimmt_nur_einen_echten_bau() {
        let root = std::env::temp_dir().join("jichi-desktop-test-bau");
        std::fs::remove_dir_all(&root).ok();

        // Ein fremdes Repository mit einer ausführbaren Datei namens jichi.
        let fremd = root.join("fremd/jichi");
        std::fs::create_dir_all(fremd.parent().unwrap()).unwrap();
        std::fs::write(&fremd, "#!/bin/sh\n").unwrap();
        // Ein gebauter Quellbaum.
        let bau = root.join("quellen");
        std::fs::create_dir_all(bau.join("include")).unwrap();
        std::fs::create_dir_all(bau.join("src")).unwrap();
        std::fs::write(bau.join("include/jc_version.h"), "").unwrap();
        std::fs::write(bau.join("src/main.c"), "").unwrap();
        std::fs::write(bau.join("jichi"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&fremd, &bau.join("jichi")] {
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        assert!(!is_jichi_build(&fremd));
        assert!(is_jichi_build(&bau.join("jichi")));
        assert_eq!(scan_for(&root, "jichi", is_jichi_build), Some(bau.join("jichi")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vorschau_liest_nur_im_projekt() {
        let root = std::env::temp_dir().join("jichi-desktop-test-vorschau");
        std::fs::remove_dir_all(&root).ok();
        let projekt = root.join("projekt");
        std::fs::create_dir_all(projekt.join("src")).unwrap();
        std::fs::write(projekt.join("src/a.txt"), "alt\n").unwrap();
        std::fs::write(root.join("geheim.txt"), "nein").unwrap();

        assert_eq!(read_workspace_file_in(&projekt, "src/a.txt").unwrap().as_deref(), Some("alt\n"));
        assert_eq!(read_workspace_file_in(&projekt, "src/neu.txt").unwrap(), None);
        assert!(read_workspace_file_in(&projekt, "../geheim.txt").is_err());
        assert!(read_workspace_file_in(&projekt, root.join("geheim.txt").to_str().unwrap()).is_err());
        assert!(read_workspace_file_in(&projekt, "../anderswo/neu.txt").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("geheim.txt"), projekt.join("link.txt")).unwrap();
            assert!(read_workspace_file_in(&projekt, "link.txt").is_err(), "Verweis nach draussen");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dokumente_eintrag_sicher_setzen_und_entfernen() {
        let root = std::env::temp_dir().join("jichi-desktop-test-docs-config");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let cfg = root.join(".jichi");
        let exe = root.join("jichi-desktop");
        std::fs::write(&cfg, r#"{"models":[{"name":"coder"}],"mcpServers":[{"name":"andere","command":"x"}]}"#).unwrap();

        docs_set_in(&cfg, &exe, true).unwrap();
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let servers = json["mcpServers"].as_array().unwrap();
        assert_eq!(servers.len(), 2, "der fremde Server bleibt");
        assert_eq!(servers[1]["name"], "dokumente");
        assert_eq!(servers[1]["args"][0], "--mcp-dokumente");
        assert_eq!(json["models"][0]["name"], "coder", "der Rest bleibt unangetastet");
        assert!(root.join(".bak-desktop").exists() || cfg.with_extension("bak-desktop").exists(), "Sicherung");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Zweimal einschalten verdoppelt nichts.
        docs_set_in(&cfg, &exe, true).unwrap();
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(json["mcpServers"].as_array().unwrap().len(), 2);
        // Ausschalten entfernt nur den eigenen Eintrag.
        docs_set_in(&cfg, &exe, false).unwrap();
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(json["mcpServers"].as_array().unwrap().len(), 1);
        assert_eq!(json["mcpServers"][0]["name"], "andere");

        // Mit Kommentaren wird nichts geschrieben.
        std::fs::write(&cfg, "{ // mein Kommentar\n \"models\": [] }").unwrap();
        assert!(docs_set_in(&cfg, &exe, true).is_err());
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("mein Kommentar"));
        // Ein Objekt statt einer Liste wird nicht umgedeutet.
        std::fs::write(&cfg, r#"{"mcpServers":{"fs":{}}}"#).unwrap();
        assert!(docs_set_in(&cfg, &exe, true).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nur_projektdateien_und_nur_harmlose_werden_geoeffnet() {
        let root = std::env::temp_dir().join("jichi-desktop-test-dateikarte");
        std::fs::remove_dir_all(&root).ok();
        let projekt = root.join("projekt");
        std::fs::create_dir_all(&projekt).unwrap();
        std::fs::write(projekt.join("bericht.pdf"), "%PDF").unwrap();
        std::fs::write(projekt.join("los.command"), "#!/bin/sh").unwrap();
        std::fs::write(root.join("draussen.pdf"), "%PDF").unwrap();
        let cwd = projekt.to_str().unwrap();

        let info = file_info_now(cwd, "bericht.pdf").unwrap();
        assert_eq!((info.name.as_str(), info.size, info.openable), ("bericht.pdf", 4, true));
        assert!(!file_info_now(cwd, "los.command").unwrap().openable, "ein Skript wird nicht geöffnet");
        assert!(file_info_now(cwd, "../draussen.pdf").is_err());
        assert!(file_info_now(cwd, "fehlt.pdf").is_err());
        assert!(file_info_now(cwd, ".").is_err(), "ein Ordner ist keine Datei");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nur_jichi_wird_als_agent_gestartet() {
        assert!(agent_program_ok(Path::new("/usr/local/bin/jichi"), &[]).is_ok());
        assert!(agent_program_ok(Path::new("/opt/x/jichi-0.12"), &[]).is_ok());
        assert!(agent_program_ok(Path::new("/bin/sh"), &["-c".into(), "rm -rf ~".into()]).is_err());
        assert!(agent_program_ok(Path::new("/usr/bin/python3"), &[]).is_err());
        if cfg!(windows) {
            assert!(agent_program_ok(Path::new("C:/Windows/System32/wsl.exe"), &["jichi".into(), "--acp".into()]).is_ok());
            assert!(agent_program_ok(Path::new("C:/Windows/System32/wsl.exe"), &["bash".into()]).is_err());
        }
    }

    #[test]
    fn anhang_nur_als_kleine_textdatei() {
        let root = std::env::temp_dir().join("jichi-desktop-test-anhang");
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("notiz.md"), "# Hallo\n").unwrap();
        std::fs::write(root.join("bild.png"), [0x89, b'P', b'N', b'G', 0, 1]).unwrap();
        std::fs::write(root.join("gross.txt"), vec![b'x'; 300 * 1024]).unwrap();

        let a = read_attachment_now(root.join("notiz.md").to_str().unwrap()).unwrap();
        assert_eq!((a.name.as_str(), a.text.as_str()), ("notiz.md", "# Hallo\n"));
        assert!(read_attachment_now(root.join("bild.png").to_str().unwrap()).is_err());
        assert!(read_attachment_now(root.join("gross.txt").to_str().unwrap()).is_err());
        assert!(read_attachment_now(root.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ungueltiges_utf8_haelt_das_lesen_nicht_an() {
        // Latin-1 in der Mitte: `lines()` hörte hier auf; gelesen werden muss bis zum Ende.
        let roh: &[u8] = b"{\"a\":1}\r\nGr\xfc\xdfe\n\n{\"b\":2}";
        let mut zeilen = Vec::new();
        for_each_line(roh, |l| zeilen.push(l));
        assert_eq!(zeilen.len(), 4, "{zeilen:?}");
        assert_eq!(zeilen[0], "{\"a\":1}");
        assert!(zeilen[1].starts_with("Gr") && zeilen[1].contains('\u{FFFD}'));
        assert_eq!(zeilen[3], "{\"b\":2}");
    }

    #[test]
    fn konfiguration_wird_gelesen_oder_als_fehlend_gemeldet() {
        let report = read_config();
        assert!(report.path.ends_with(".jichi"));
        if report.exists && report.problem.is_none() {
            // Wenn es sie gibt, muss sie brauchbar sein: Modelle mit Namen,
            // und der Schlüssel als *Name* einer Variablen, nie als Wert.
            for m in &report.models {
                assert!(!m.name.is_empty(), "Modell ohne Namen");
                assert!(
                    !m.api_key_env.as_deref().unwrap_or("").contains("sk-"),
                    "in apiKeyEnv steht ein Schlüssel statt eines Variablennamens"
                );
            }
        }
    }

    #[test]
    fn schluessel_wird_abgelegt_gelesen_und_entfernt() {
        eigene_ablage();
        let konto = "TEST_KEY";
        secret_forget(konto.into()).unwrap();
        assert!(!secret_noted(konto), "vorher ist nichts da");
        assert!(!legacy_secret_allowed(konto), "alte Dateien bleiben nach Entfernen gesperrt");

        secret_store(konto.into(), "  geheim-12345\n".into()).unwrap();
        assert!(secret_noted(konto));
        assert!(legacy_secret_allowed(konto), "ein neuer Schlüssel hebt die Sperre auf");

        // Über den Weg, den auch der Start nimmt -- und getrimmt.
        let env = resolve_env(&[EnvSpec {
            name: "JICHI_API_KEY".into(),
            secret: Some(konto.into()),
            value: None,
            file: None,
        }])
        .unwrap();
        assert_eq!(env.get("JICHI_API_KEY").map(String::as_str), Some("geheim-12345"));

        // Nur der Benutzer darf lesen. Das ist der ganze Schutz dieser Ablage,
        // also wird er geprüft und nicht angenommen.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = secret_path(konto).unwrap().metadata().unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "Rechte sind {:o}", mode & 0o777);
        }

        secret_forget(konto.into()).unwrap();
        assert!(!secret_noted(konto));
        assert!(!legacy_secret_allowed(konto));
        secret_forget(konto.into()).unwrap(); // zweimal ist kein Fehler
        assert!(secret_store(konto.into(), "   ".into()).is_err(), "leer wird abgelehnt");
    }

    #[test]
    fn ein_name_mit_pfadtrenner_wird_abgelehnt() {
        eigene_ablage();
        // Sonst schriebe ein Aufruf irgendwohin ins Dateisystem.
        for boese in ["../../etc/passwd", "a/b", "..", ""] {
            assert!(secret_path(boese).is_none(), "{boese} wurde angenommen");
        }
    }

    /// `doctor --output json` gegen den echten Agenten.
    #[test]
    #[ignore = "braucht ein gebautes jichi, Konfiguration und Netz"]
    fn doctor_liefert_einen_bericht() {
        let bericht = doctor_now(
            "jichi".into(),
            Some(vec![EnvSpec {
                name: "JICHI_API_KEY".into(),
                secret: None,
                value: None,
                file: Some("~/.config/jlu/apikey.txt".into()),
            }]),
        )
        .expect("Bericht");

        let ok = bericht.get("ok").and_then(|v| v.as_u64()).unwrap_or(0);
        let fail = bericht.get("fail").and_then(|v| v.as_u64()).unwrap_or(999);
        println!("ok={ok} fail={fail}");
        assert!(ok > 5, "zu wenige bestandene Prüfungen: {ok}");
        assert_eq!(fail, 0, "der Agent meldet Fehler: {bericht}");

        // Die Prüfungen, an denen der erste Start hängt.
        let labels: Vec<String> = bericht
            .get("checks")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("label").and_then(|l| l.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(labels.iter().any(|l| l.contains("API key present")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("server reachable")), "{labels:?}");
    }

    #[test]
    fn bereitschaft_beantwortet_die_eine_frage() {
        eigene_ablage();
        let r = readiness_now(None);
        assert_eq!(r.key_env, "JICHI_API_KEY");
        // Fehlt irgendetwas, muss der erste Start verlangt werden.
        let fehlt = r.agent.is_none() || !r.config.exists || r.config.models.is_empty() || !r.key_stored;
        assert_eq!(r.needs_setup, fehlt);
    }

    /// Der Weg, den das Live-Gespräch nimmt: Schlüssel aus der Ablage lesen,
    /// als Kopfzeile setzen, Handschlag mit der Bridge.
    ///
    /// Das ist die Prüfung, die der Browser nicht bestehen *kann* — sein
    /// WebSocket schickt keine Kopfzeilen und bekommt darum 401. Allein
    /// starten, damit keine andere Prüfung die Ablage umlenkt:
    ///
    ///     cargo test tests::live_handschlag -- --ignored --exact --nocapture
    #[tokio::test]
    #[ignore = "braucht die echte Ablage, das VPN und eine laufende Bridge"]
    async fn live_handschlag() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        // Die echte Ablage, nicht das Testverzeichnis.
        std::env::remove_var("JICHI_DESKTOP_DIR");

        let schluessel = secret_read("BRIDGE_API_KEY")
            .expect("BRIDGE_API_KEY liegt in der Ablage");
        assert!(schluessel.len() >= 32, "Schlüssel wirkt zu kurz");

        let url = "wss://sv90146.hrz.uni-giessen.de/v1/realtime";

        // Erst der Gegenbeweis: ohne Kopfzeile muss es scheitern.
        let ohne = tokio_tungstenite::connect_async(url.into_client_request().unwrap()).await;
        assert!(ohne.is_err(), "ohne Schlüssel darf die Bridge nicht verbinden");
        println!("ohne Kopfzeile: abgewiesen (erwartet)");

        // Und nun der Weg der Anwendung.
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {schluessel}").parse().unwrap());
        let (socket, antwort) = tokio_tungstenite::connect_async(request)
            .await
            .expect("mit Schlüssel verbindet die Bridge");
        println!("mit Kopfzeile: {} ", antwort.status());

        use futures_util::StreamExt;
        let (_, mut leser) = socket.split();
        // Die Bridge meldet sich von selbst mit session.created.
        let erste = tokio::time::timeout(std::time::Duration::from_secs(15), leser.next())
            .await
            .expect("die Bridge meldet sich")
            .expect("Nachricht")
            .expect("gültige Nachricht");
        let text = erste.into_text().expect("Text");
        println!("erste Nachricht: {}", &text[..text.len().min(120)]);
        assert!(text.contains("session"), "erwartet wurde session.created: {text}");
    }

    /// Der ganze Weg gegen den echten Agenten: finden, Schlüssel aus der Datei
    /// setzen, starten, ACP sprechen.
    ///
    /// Übersprungen, solange nicht ausdrücklich verlangt — er braucht ein
    /// gebautes jichi und eine Schlüsseldatei, und beides gehört nicht zu den
    /// Voraussetzungen dieses Projekts:
    ///
    ///     cargo test -- --ignored --nocapture
    #[test]
    #[ignore = "braucht ein gebautes jichi und eine Schlüsseldatei"]
    fn echter_agent_antwortet() {
        let Some(path) = which("jichi") else {
            panic!("jichi wurde nicht gefunden — genau das prüft dieser Test");
        };
        println!("gefunden: {}", path.display());

        let env = resolve_env(&[EnvSpec {
            name: "JICHI_API_KEY".into(), secret: None,
            value: None,
            file: Some("~/.config/jlu/apikey.txt".into()),
        }])
        .expect("Schlüsseldatei lesbar");
        assert_eq!(env.len(), 1);

        let mut child = Command::new(&path)
            .arg("--acp")
            .env("PATH", child_path())
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("jichi startet");

        {
            let stdin = child.stdin.as_mut().unwrap();
            writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1,"clientCapabilities":{{}}}}}}"#
            )
            .unwrap();
            writeln!(
                stdin,
                r#"{{"jsonrpc":"2.0","id":2,"method":"session/new","params":{{"cwd":"/tmp","mcpServers":[]}}}}"#
            )
            .unwrap();
        }
        // stdin schliessen -> der Agent sieht EOF und beendet sich.
        drop(child.stdin.take());

        let out = child.wait_with_output().expect("jichi endet");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        println!("stdout:\n{stdout}");
        println!("stderr:\n{stderr}");

        let zeilen: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(zeilen.len(), 2, "zwei Antworten erwartet");
        assert!(zeilen[0].contains(r#""protocolVersion":1"#), "{}", zeilen[0]);
        assert!(zeilen[1].contains(r#""sessionId""#), "{}", zeilen[1]);

        // Der eigentliche Punkt: ohne den Schlüssel aus der Datei stünde hier
        // "no API key found". Steht er da, wäre jeder Zug später gescheitert.
        assert!(
            !stderr.contains("no API key found"),
            "der Schlüssel kam nicht an: {stderr}"
        );
    }

    #[test]
    fn die_suche_meidet_bewachte_ordner() {
        // macOS fragt für jeden dieser Ordner einzeln um Erlaubnis. Die Suche
        // darf sie nie betreten, sonst klickt der Benutzer bei jedem Start.
        for bewacht in [
            "Desktop", "Documents", "Downloads", "Music", "Movies", "Pictures",
            "Photos", "Library", "Applications", "Mobile Documents",
        ] {
            assert!(skip_dir(bewacht), "{bewacht} wird betreten");
        }
        assert!(skip_dir(".ssh"), "versteckte Ordner werden betreten");
        // Und ein gewöhnlicher Projektordner muss weiter durchsucht werden.
        assert!(!skip_dir("projekte"));
        assert!(!skip_dir("FOLDER1HOME"));
    }

    #[test]
    fn die_suche_gibt_rechtzeitig_auf() {
        eigene_ablage();
        // Ein Baum, in dem es nichts zu finden gibt: die Suche muss innerhalb
        // ihres Zeitbudgets zurückkommen, nicht erst wenn sie fertig ist.
        let start = std::time::Instant::now();
        let _ = scan_for(&home().unwrap(), "gibt-es-hier-ganz-sicher-nicht-4711", |_| true);
        let gebraucht = start.elapsed();
        assert!(
            gebraucht < SCAN_MAX_TIME + std::time::Duration::from_secs(2),
            "die Suche lief {gebraucht:?}, erlaubt sind {SCAN_MAX_TIME:?}"
        );
    }

    #[test]
    fn startvorschlag_ist_vollstaendig() {
        eigene_ablage();
        let launch = default_launch_now();
        assert!(!launch.program.is_empty());
        assert!(launch.args.contains(&"--acp".to_string()));
        // Ein absoluter Pfad ist Pflicht: ACP verlangt ihn für session/new.
        assert!(launch.cwd.starts_with('/') || launch.cwd.contains(':'), "{}", launch.cwd);
    }
}
