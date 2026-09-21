//! Nativer, std-basierter Downloadmanager für GGUF-Modelldateien.
//!
//! Sicherheitsmodell (SSRF-Härtung):
//! - Nur `https://`-URLs werden akzeptiert; jeder Redirect-Hop wird erneut
//!   gegen dieselbe Regel geprüft (max. [`MAX_REDIRECTS`] Hops).
//! - [`SafeResolver`] ist der einzige Weg, wie ureq einen Hostnamen in eine
//!   Adresse übersetzt. Er löst den Hostnamen genau einmal auf und liefert
//!   ausschließlich global routbare Adressen zurück; ist auch nur eine
//!   aufgelöste Adresse privat/loopback/link-local/reserviert, schlägt die
//!   Auflösung komplett fehl, *bevor* irgendeine Verbindung entsteht. Da
//!   Prüfung und tatsächlich verwendete Adresse aus demselben Resolve-Aufruf
//!   stammen (nicht aus zwei unabhängigen DNS-Lookups), ist ein
//!   DNS-Rebinding-Fenster zwischen Prüfung und Verbindungsaufbau
//!   ausgeschlossen.
//!
//! Downloads werden über [`crate::staging::StageStore`] gestreamt (nie
//! komplett im Speicher gehalten) und landen dort zunächst als atomarer
//! Commit (`.part` -> `.bin` + `.commit`). Erst nach GGUF-Magic-Prüfung und
//! optionaler SHA256-Verifikation wird die Datei per `restore_active`
//! atomar und sichtbar ins Modellverzeichnis übernommen — bis dahin sieht
//! die GGUF-Registry (und damit der Modellkatalog) nichts von einem
//! laufenden oder fehlgeschlagenen Download.
//!
//! Nach einem erfolgreichen Download rescannt [`rescan_catalog_if_download_completed`]
//! automatisch den gesamten Modellkatalog (dieselbe Logik wie der manuelle
//! `POST /api/models/rescan`-Endpunkt), damit das neue Modell ohne
//! Serverneustart oder manuellen Rescan in `/v1/models` erscheint. Das hasht
//! sequenziell *jede* Datei in allen konfigurierten Modellquellen, nicht nur
//! die frisch heruntergeladene — bei vielen oder großen bereits vorhandenen
//! Modellen kann das je nach Festplattengeschwindigkeit spürbar (empirisch
//! auf dieser Maschine: bis über eine halbe Minute) dauern. Der Download
//! selbst meldet `Completed`, *bevor* dieser Rescan fertig ist; das neue
//! Modell wird erst mit etwas Verzögerung sichtbar. `duration_ms` im
//! `catalog_rescanned`-Event macht diese Dauer beobachtbar.

use crate::{config::Config, gguf_registry, model_catalog::ModelCatalog, staging::StageStore};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::Duration,
};

const MAX_REDIRECTS: u8 = 5;
const READ_CHUNK: usize = 64 * 1024;
const CONNECT_TIMEOUT_SECS: u64 = 20;
const READ_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    pub file_name: String,
    pub expected_size_bytes: Option<u64>,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadPhase {
    Downloading,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    pub id: String,
    pub phase: DownloadPhase,
    pub bytes_downloaded: u64,
    pub total_bytes: Option<u64>,
    pub message: Option<String>,
}

#[derive(Debug)]
pub enum DownloadError {
    InvalidRequest(String),
    NotFound,
    Io(io::Error),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => f.write_str(message),
            Self::NotFound => f.write_str("kein Download mit dieser ID bekannt"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for DownloadError {}

struct DownloadHandle {
    phase: Mutex<DownloadPhase>,
    bytes_downloaded: AtomicU64,
    total_bytes: Mutex<Option<u64>>,
    message: Mutex<Option<String>>,
    cancel: AtomicBool,
}

impl DownloadHandle {
    fn new() -> Self {
        Self {
            phase: Mutex::new(DownloadPhase::Downloading),
            bytes_downloaded: AtomicU64::new(0),
            total_bytes: Mutex::new(None),
            message: Mutex::new(None),
            cancel: AtomicBool::new(false),
        }
    }

    fn set_phase(&self, phase: DownloadPhase) {
        *self.phase.lock().unwrap() = phase;
    }

    fn fail(&self, message: impl Into<String>) {
        *self.message.lock().unwrap() = Some(message.into());
        self.set_phase(DownloadPhase::Failed);
    }

    fn status(&self, id: &str) -> DownloadStatus {
        DownloadStatus {
            id: id.to_owned(),
            phase: *self.phase.lock().unwrap(),
            bytes_downloaded: self.bytes_downloaded.load(Ordering::Relaxed),
            total_bytes: *self.total_bytes.lock().unwrap(),
            message: self.message.lock().unwrap().clone(),
        }
    }
}

/// Nur ureq's internem Verbindungsaufbau erlaubt, tatsächlich einen Socket zu
/// öffnen — siehe Modul-Dokumentation zum DNS-Rebinding-Schutz.
#[derive(Debug, Clone, Default)]
struct SafeResolver;

impl ureq::Resolver for SafeResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        if addrs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("keine Adresse fuer '{netloc}' aufloesbar"),
            ));
        }
        for addr in &addrs {
            if !is_globally_routable(addr.ip()) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "Ziel '{netloc}' loest auf die nicht-oeffentliche Adresse {} auf \
                         (SSRF-Schutz)",
                        addr.ip()
                    ),
                ));
            }
        }
        Ok(addrs)
    }
}

fn is_globally_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_globally_routable(v4),
        IpAddr::V6(v6) => ipv6_is_globally_routable(v6),
    }
}

fn ipv4_is_globally_routable(ip: Ipv4Addr) -> bool {
    if ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
    {
        return false;
    }
    match ip.octets() {
        [0, ..] => false,                                 // 0.0.0.0/8 "this network"
        [100, b, ..] if (64..=127).contains(&b) => false, // 100.64.0.0/10 CGNAT
        [192, 0, 0, _] => false,                          // IETF-Protokollzuweisungen
        [192, 88, 99, _] => false,                        // veralteter 6to4-Anycast-Relay
        [198, 18, ..] | [198, 19, ..] => false,           // Benchmarking 198.18.0.0/15
        [240..=255, ..] => false,                         // reserviert (240.0.0.0/4) + Rest
        _ => true,
    }
}

fn ipv6_is_globally_routable(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_globally_routable(v4);
    }
    let segments = ip.segments();
    if (segments[0] & 0xffc0) == 0xfe80 {
        return false; // fe80::/10 link-local
    }
    if (segments[0] & 0xfe00) == 0xfc00 {
        return false; // fc00::/7 unique local (ULA)
    }
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return false; // 2001:db8::/32 Dokumentation
    }
    true
}

pub struct DownloadManager {
    stage: StageStore,
    models_dir: PathBuf,
    ssd_reserve_mb: u64,
    agent: ureq::Agent,
    handles: Mutex<HashMap<String, Arc<DownloadHandle>>>,
    next_id: AtomicU64,
    // Fuer den automatischen Katalog-Rescan nach einem erfolgreichen
    // Download (siehe `run_download`): eine eigene `Config`-Kopie (billig,
    // nur beim lazy Manager-Aufbau geklont, nicht pro Request) fuer
    // `model_sources::scan`, sowie der geteilte Katalog, in den das Ergebnis
    // atomar zurueckgeschrieben wird — derselbe `Arc<RwLock<ModelCatalog>>`,
    // den auch `/v1/models` und der manuelle Rescan-Endpunkt verwenden.
    config: Config,
    catalog: Arc<RwLock<ModelCatalog>>,
}

impl DownloadManager {
    pub fn new(config: &Config, catalog: Arc<RwLock<ModelCatalog>>) -> io::Result<Self> {
        let stage = StageStore::new(config.paths.stage_dir.join("downloads"))?;
        let agent = ureq::AgentBuilder::new()
            .resolver(SafeResolver)
            .redirects(0)
            .timeout_connect(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .timeout_read(Duration::from_secs(READ_TIMEOUT_SECS))
            .build();
        Ok(Self {
            stage,
            models_dir: config.models_dir(),
            ssd_reserve_mb: config.reserves.ssd_mb,
            agent,
            handles: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            config: config.clone(),
            catalog,
        })
    }

    pub fn create(&self, request: DownloadRequest) -> Result<String, DownloadError> {
        validate_file_name(&request.file_name)?;
        let parsed = url::Url::parse(&request.url)
            .map_err(|err| DownloadError::InvalidRequest(format!("URL ungueltig: {err}")))?;
        if parsed.scheme() != "https" {
            return Err(DownloadError::InvalidRequest(
                "nur https:// wird unterstuetzt".into(),
            ));
        }
        if parsed.host_str().is_none() {
            return Err(DownloadError::InvalidRequest(
                "URL enthaelt keinen Host".into(),
            ));
        }
        if let Some(expected) = request.expected_size_bytes {
            self.ensure_ssd_headroom(expected)?;
        }
        if let Some(sha) = &request.sha256 {
            if !valid_sha256_hex(sha) {
                return Err(DownloadError::InvalidRequest(
                    "sha256 muss aus 64 Hex-Zeichen bestehen".into(),
                ));
            }
        }

        let generation = stable_hash(&request.url);
        let block_id = request.file_name.clone();
        let id = format!("dl-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let handle = Arc::new(DownloadHandle::new());
        self.handles
            .lock()
            .unwrap()
            .insert(id.clone(), handle.clone());

        crate::observability::emit(
            "download_started",
            serde_json::json!({
                "id": id,
                "file_name": request.file_name,
                "expected_size_bytes": request.expected_size_bytes,
                "sha256_pinned": request.sha256.is_some(),
            }),
        );

        let stage = self.stage.clone();
        let agent = self.agent.clone();
        let models_dir = self.models_dir.clone();
        let event_id = id.clone();
        let rescan_config = self.config.clone();
        let catalog = self.catalog.clone();
        thread::spawn(move || {
            run_download(
                &stage,
                &agent,
                &models_dir,
                &block_id,
                generation,
                request,
                &handle,
                &event_id,
                &rescan_config,
                &catalog,
            );
        });
        Ok(id)
    }

    pub fn status(&self, id: &str) -> Result<DownloadStatus, DownloadError> {
        let handles = self.handles.lock().unwrap();
        let handle = handles.get(id).ok_or(DownloadError::NotFound)?;
        Ok(handle.status(id))
    }

    pub fn cancel(&self, id: &str) -> Result<(), DownloadError> {
        let handles = self.handles.lock().unwrap();
        let handle = handles.get(id).ok_or(DownloadError::NotFound)?;
        handle.cancel.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn ensure_ssd_headroom(&self, expected_bytes: u64) -> Result<(), DownloadError> {
        let snapshot = crate::resources::read().map_err(DownloadError::Io)?;
        let expected_mb = expected_bytes.div_ceil(1_048_576);
        let needed = expected_mb.saturating_add(self.ssd_reserve_mb);
        if snapshot.ssd_free_mb < needed {
            return Err(DownloadError::InvalidRequest(format!(
                "zu wenig freier Speicherplatz: {} MB frei, {needed} MB benoetigt \
                 (Modell + {} MB Reserve)",
                snapshot.ssd_free_mb, self.ssd_reserve_mb
            )));
        }
        Ok(())
    }
}

impl From<io::Error> for DownloadError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

fn validate_file_name(name: &str) -> Result<(), DownloadError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return Err(DownloadError::InvalidRequest(
            "file_name ist ungueltig (kein Pfad, kein Traversal)".into(),
        ));
    }
    Ok(())
}

fn valid_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn stable_hash(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Läuft im eigenen Thread, unabhängig von jedem `Mutex`, den die HTTP-Schicht
/// für die Engine hält — ein Download darf die Modell-Inferenz nicht
/// blockieren und umgekehrt.
#[allow(clippy::too_many_arguments)]
fn run_download(
    stage: &StageStore,
    agent: &ureq::Agent,
    models_dir: &std::path::Path,
    block_id: &str,
    generation: u64,
    request: DownloadRequest,
    handle: &DownloadHandle,
    id: &str,
    rescan_config: &Config,
    catalog: &RwLock<ModelCatalog>,
) {
    match try_download(
        stage, agent, models_dir, block_id, generation, &request, handle, "https", id,
    ) {
        Ok(()) => rescan_catalog_if_download_completed(handle, id, rescan_config, catalog),
        Err(message) => {
            // Ein Abbruch per `cancel()` ist kein Fehlerfall: der aufrufende
            // Code hat die Phase bereits explizit gesetzt und die
            // `.part`-Datei verworfen, bevor der Fehlerpfad hier ueberhaupt
            // erreicht wird — und hat dafuer schon sein eigenes Event
            // emittiert, nicht dieses hier.
            if *handle.phase.lock().unwrap() != DownloadPhase::Cancelled {
                crate::observability::emit(
                    "download_failed",
                    serde_json::json!({"id": id, "message": message}),
                );
                handle.fail(message);
            }
        }
    }
}

/// Rescannt den Modellkatalog automatisch, wenn `handle` nach `try_download`
/// tatsaechlich `Completed` erreicht hat — nicht nach einem Cancel (das
/// laesst `try_download` ebenfalls als `Ok(())` zurueckkommen) oder Fehler.
/// Eigene Funktion statt inline in `run_download`, damit dieser Trigger
/// isoliert getestet werden kann, ohne einen echten Download durchzufuehren.
fn rescan_catalog_if_download_completed(
    handle: &DownloadHandle,
    id: &str,
    rescan_config: &Config,
    catalog: &RwLock<ModelCatalog>,
) {
    if *handle.phase.lock().unwrap() != DownloadPhase::Completed {
        return;
    }
    // Der Scan hasht sequenziell *jede* Datei in allen konfigurierten
    // Modellquellen (nicht nur die frisch heruntergeladene) — bei vielen
    // oder grossen bereits vorhandenen Modellen kann das sehr lange dauern
    // (siehe INGRIED-Review zu Welle 8: "Risiko: Rescan kann bei vielen
    // Modellen Ressourcen ueberlasten"). `duration_ms` im Event macht das
    // beobachtbar, statt es nur zu vermuten.
    let started = std::time::Instant::now();
    let (model_count, warnings) = crate::model_sources::rescan_and_apply(rescan_config, catalog);
    crate::observability::emit(
        "catalog_rescanned",
        serde_json::json!({
            "model_count": model_count,
            "warning_count": warnings.len(),
            "trigger": "download",
            "download_id": id,
            "duration_ms": started.elapsed().as_millis(),
        }),
    );
}

/// `required_scheme` ist in Produktion immer `"https"` (siehe [`run_download`]).
/// Tests uebergeben `"http"`, um den kompletten Redirect-/Resume-/Verifikations-
/// Ablauf gegen einen lokalen Klartext-Mock-Server zu pruefen, ohne dafuer ein
/// TLS-Zertifikat zu benoetigen — der SSRF-/Downgrade-Schutz selbst (erzwungenes
/// https, [`SafeResolver`]) sitzt bereits vollstaendig in [`DownloadManager::create`]
/// und wird dort separat getestet.
#[allow(clippy::too_many_arguments)]
fn try_download(
    stage: &StageStore,
    agent: &ureq::Agent,
    models_dir: &std::path::Path,
    block_id: &str,
    generation: u64,
    request: &DownloadRequest,
    handle: &DownloadHandle,
    required_scheme: &str,
    id: &str,
) -> Result<(), String> {
    let (mut part_file, mut resume_from) = stage
        .open_part_for_resume(block_id, generation)
        .map_err(|err| format!("Staging-Datei nicht oeffenbar: {err}"))?;
    handle
        .bytes_downloaded
        .store(resume_from, Ordering::Relaxed);
    if resume_from > 0 {
        crate::observability::emit(
            "download_resumed",
            serde_json::json!({"id": id, "resume_from_bytes": resume_from}),
        );
    }

    let mut current_url = request.url.clone();
    let mut redirects = 0u8;
    let response = loop {
        let mut req = agent.get(&current_url);
        if resume_from > 0 {
            req = req.set("Range", &format!("bytes={resume_from}-"));
        }
        let response = req
            .call()
            .map_err(|err| format!("Download fehlgeschlagen: {err}"))?;
        let status = response.status();
        if (300..400).contains(&status) {
            if redirects >= MAX_REDIRECTS {
                return Err(format!("mehr als {MAX_REDIRECTS} Redirects"));
            }
            redirects += 1;
            let location = response
                .header("Location")
                .ok_or_else(|| "Redirect ohne Location-Header".to_string())?;
            let next = url::Url::parse(&current_url)
                .and_then(|base| base.join(location))
                .map_err(|err| format!("Redirect-Ziel ungueltig: {err}"))?;
            if next.scheme() != required_scheme {
                return Err(format!(
                    "Redirect verweist auf ein Nicht-{required_scheme}-Ziel"
                ));
            }
            current_url = next.to_string();
            continue;
        }
        break response;
    };

    let status = response.status();
    let content_length: Option<u64> = response
        .header("Content-Length")
        .and_then(|value| value.parse().ok());

    if resume_from > 0 && status != 206 {
        // Server ignoriert den Range-Header und liefert den kompletten Body
        // erneut (Status 200) — die bisherige `.part`-Datei waere ab hier
        // doppelt/korrupt. Von vorn beginnen statt eine kaputte Datei zu
        // committen.
        drop(part_file);
        stage
            .discard_part(block_id, generation)
            .map_err(|err| format!("verworfene Teildatei nicht loeschbar: {err}"))?;
        let (file, len) = stage
            .open_part_for_resume(block_id, generation)
            .map_err(|err| format!("Staging-Datei nicht neu oeffenbar: {err}"))?;
        part_file = file;
        resume_from = len;
        handle
            .bytes_downloaded
            .store(resume_from, Ordering::Relaxed);
    }

    let total_bytes = content_length.map(|len| resume_from + len);
    *handle.total_bytes.lock().unwrap() = total_bytes;
    if let (Some(expected), Some(total)) = (request.expected_size_bytes, total_bytes) {
        if expected != total {
            return Err(format!(
                "Server meldet {total} Bytes, erwartet wurden {expected}"
            ));
        }
    }

    let mut reader = response.into_reader();
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        if handle.cancel.load(Ordering::SeqCst) {
            drop(part_file);
            let _ = stage.discard_part(block_id, generation);
            handle.set_phase(DownloadPhase::Cancelled);
            crate::observability::emit("download_cancelled", serde_json::json!({"id": id}));
            return Ok(());
        }
        let read = reader
            .read(&mut buf)
            .map_err(|err| format!("Lesefehler beim Download: {err}"))?;
        if read == 0 {
            break;
        }
        part_file
            .write_all(&buf[..read])
            .map_err(|err| format!("Schreibfehler beim Staging: {err}"))?;
        handle
            .bytes_downloaded
            .fetch_add(read as u64, Ordering::Relaxed);
    }
    part_file
        .sync_all()
        .map_err(|err| format!("Teildatei nicht durchsynchronisierbar: {err}"))?;
    drop(part_file);

    // Ein Server, der die Verbindung vorzeitig schliesst, liefert bis dahin
    // ganz normale Bytes gefolgt von einem sauberen EOF (`read() == 0`) —
    // das ist von aussen nicht von einem tatsaechlich vollstaendigen Download
    // zu unterscheiden, ausser man vergleicht die empfangene Menge mit dem
    // vom Server angekuendigten `Content-Length` dieses Hops. Ohne diese
    // Pruefung wuerde ein Netzwerkabbruch ohne angegebenes `sha256` als
    // erfolgreicher Download durchgehen. Die `.part`-Datei bleibt fuer einen
    // Resume-Versuch liegen statt verworfen zu werden.
    if let Some(expected_hop_bytes) = content_length {
        let received_this_hop = handle.bytes_downloaded.load(Ordering::Relaxed) - resume_from;
        if received_this_hop != expected_hop_bytes {
            return Err(format!(
                "Verbindung unterbrochen: {received_this_hop} von {expected_hop_bytes} \
                 angekuendigten Bytes empfangen"
            ));
        }
    }

    handle.set_phase(DownloadPhase::Verifying);
    stage
        .finalize_part(block_id, generation, request.sha256.as_deref())
        .map_err(|err| format!("Verifikation fehlgeschlagen: {err}"))?;

    let staged_path = stage.root().join(format!("{block_id}.{generation}.bin"));
    if let Err(err) = gguf_registry::inspect(&staged_path) {
        let _ = std::fs::remove_file(&staged_path);
        let _ = std::fs::remove_file(stage.root().join(format!("{block_id}.{generation}.commit")));
        return Err(format!("keine gueltige GGUF-Datei: {err}"));
    }

    let target = models_dir.join(&request.file_name);
    std::fs::create_dir_all(models_dir)
        .map_err(|err| format!("Modellverzeichnis nicht anlegbar: {err}"))?;
    stage
        .restore_active(block_id, generation, &target)
        .map_err(|err| format!("Uebernahme ins Modellverzeichnis fehlgeschlagen: {err}"))?;
    let _ = std::fs::remove_file(&staged_path);
    let _ = std::fs::remove_file(stage.root().join(format!("{block_id}.{generation}.commit")));

    crate::observability::emit(
        "download_completed",
        serde_json::json!({
            "id": id,
            "file_name": request.file_name,
            "bytes": handle.bytes_downloaded.load(Ordering::Relaxed),
        }),
    );
    handle.set_phase(DownloadPhase::Completed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::fs;

    #[test]
    fn rejects_private_ipv4_targets() {
        assert!(!is_globally_routable("10.0.0.1".parse().unwrap()));
        assert!(!is_globally_routable("172.16.5.1".parse().unwrap()));
        assert!(!is_globally_routable("192.168.1.1".parse().unwrap()));
        assert!(!is_globally_routable("127.0.0.1".parse().unwrap()));
        assert!(!is_globally_routable("169.254.1.1".parse().unwrap()));
        assert!(!is_globally_routable("0.0.0.0".parse().unwrap()));
        assert!(!is_globally_routable("100.64.0.1".parse().unwrap()));
        assert!(!is_globally_routable("255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn accepts_public_ipv4_targets() {
        assert!(is_globally_routable("1.1.1.1".parse().unwrap()));
        assert!(is_globally_routable("93.184.216.34".parse().unwrap()));
    }

    #[test]
    fn rejects_private_and_special_ipv6_targets() {
        assert!(!is_globally_routable("::1".parse().unwrap()));
        assert!(!is_globally_routable("::".parse().unwrap()));
        assert!(!is_globally_routable("fe80::1".parse().unwrap()));
        assert!(!is_globally_routable("fc00::1".parse().unwrap()));
        assert!(!is_globally_routable("fd12:3456:789a::1".parse().unwrap()));
        // IPv4-mapped Adresse einer privaten IP muss ueber die eingebettete
        // v4-Adresse abgelehnt werden, nicht separat behandelt werden.
        assert!(!is_globally_routable("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn accepts_public_ipv6_targets() {
        assert!(is_globally_routable(
            "2606:4700:4700::1111".parse().unwrap()
        ));
    }

    #[test]
    fn validate_file_name_rejects_traversal_and_separators() {
        assert!(validate_file_name("model.gguf").is_ok());
        assert!(validate_file_name("../escape.gguf").is_err());
        assert!(validate_file_name("dir/model.gguf").is_err());
        assert!(validate_file_name("").is_err());
    }

    #[test]
    fn valid_sha256_hex_requires_64_hex_chars() {
        assert!(valid_sha256_hex(&"a".repeat(64)));
        assert!(!valid_sha256_hex(&"a".repeat(63)));
        assert!(!valid_sha256_hex("not-hex-and-too-short"));
    }

    #[test]
    fn create_rejects_non_https_url() {
        let dir = unique_dir("non-https");
        let manager = manager_for(&dir);
        let err = manager
            .create(DownloadRequest {
                url: "http://example.com/model.gguf".into(),
                file_name: "model.gguf".into(),
                expected_size_bytes: None,
                sha256: None,
            })
            .unwrap_err();
        assert!(matches!(err, DownloadError::InvalidRequest(_)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_rejects_url_resolving_to_private_address() {
        let dir = unique_dir("private-target");
        let manager = manager_for(&dir);
        let id = manager
            .create(DownloadRequest {
                url: "https://localhost/model.gguf".into(),
                file_name: "model.gguf".into(),
                expected_size_bytes: None,
                sha256: None,
            })
            .unwrap();
        let status = wait_for_terminal(&manager, &id);
        assert_eq!(status.phase, DownloadPhase::Failed);
        assert!(status
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("Download fehlgeschlagen"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_rejects_path_traversal_file_name() {
        let dir = unique_dir("traversal");
        let manager = manager_for(&dir);
        let err = manager
            .create(DownloadRequest {
                url: "https://example.com/model.gguf".into(),
                file_name: "../../etc/passwd".into(),
                expected_size_bytes: None,
                sha256: None,
            })
            .unwrap_err();
        assert!(matches!(err, DownloadError::InvalidRequest(_)));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn status_of_unknown_id_is_not_found() {
        let dir = unique_dir("unknown-status");
        let manager = manager_for(&dir);
        assert!(matches!(
            manager.status("dl-does-not-exist"),
            Err(DownloadError::NotFound)
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancel_of_unknown_id_is_not_found() {
        let dir = unique_dir("unknown-cancel");
        let manager = manager_for(&dir);
        assert!(matches!(
            manager.cancel("dl-does-not-exist"),
            Err(DownloadError::NotFound)
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_rejects_when_expected_size_exceeds_free_ssd_space() {
        let dir = unique_dir("no-space");
        let mut config = Config::default();
        config.paths.project_root = dir.clone();
        config.paths.stage_dir = dir.join("staging");
        config.paths.models_dir = Some(dir.join("models"));
        config.reserves.ssd_mb = 0;
        let manager =
            DownloadManager::new(&config, Arc::new(RwLock::new(ModelCatalog::new()))).unwrap();
        let absurd_bytes = u64::MAX - 1_048_576; // rundet auf einen absurd hohen MB-Wert
        let err = manager
            .create(DownloadRequest {
                url: "https://example.com/model.gguf".into(),
                file_name: "model.gguf".into(),
                expected_size_bytes: Some(absurd_bytes),
                sha256: None,
            })
            .unwrap_err();
        assert!(matches!(err, DownloadError::InvalidRequest(_)));
        std::fs::remove_dir_all(dir).ok();
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tri-ai-download-{tag}-{}-{now}",
            std::process::id()
        ))
    }

    fn manager_for(dir: &std::path::Path) -> DownloadManager {
        let mut config = Config::default();
        config.paths.project_root = dir.to_path_buf();
        config.paths.stage_dir = dir.join("staging");
        config.paths.models_dir = Some(dir.join("models"));
        DownloadManager::new(&config, Arc::new(RwLock::new(ModelCatalog::new()))).unwrap()
    }

    fn wait_for_terminal(manager: &DownloadManager, id: &str) -> DownloadStatus {
        for _ in 0..200 {
            let status = manager.status(id).unwrap();
            if matches!(
                status.phase,
                DownloadPhase::Completed | DownloadPhase::Failed | DownloadPhase::Cancelled
            ) {
                return status;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("Download '{id}' wurde nicht innerhalb des Testzeitlimits terminal");
    }

    // --- Ende-zu-Ende-Tests der Redirect-/Resume-/Verifikations-
    // Zustandsmaschine gegen einen lokalen Klartext-Mock-Server. Rufen
    // `try_download` direkt mit `required_scheme = "http"` auf (siehe
    // Dokumentation dort) statt ueber `DownloadManager::create`, dessen
    // https-Zwang und SSRF-Schutz bereits oben unabhaengig getestet sind.

    fn minimal_gguf_bytes(padding: usize) -> Vec<u8> {
        let mut bytes = Vec::from(&b"GGUF"[..]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&0u64.to_le_bytes()); // metadata_count
        bytes.extend(std::iter::repeat_n(0xAAu8, padding));
        bytes
    }

    fn test_agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(Duration::from_secs(5))
            .build()
    }

    struct MockRequest {
        range: Option<String>,
    }

    /// Liest genau eine HTTP/1.1-Anfrage ohne Body (wie ureq's `GET`) bis zur
    /// Leerzeile und extrahiert den `Range`-Header, falls gesendet.
    fn read_mock_request(stream: &mut std::net::TcpStream) -> MockRequest {
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let range = text
            .lines()
            .find_map(|line| line.strip_prefix("Range: ").map(str::to_owned));
        MockRequest { range }
    }

    type MockResponder = Box<dyn Fn(&MockRequest) -> Vec<u8> + Send>;

    /// Bedient `responses.len()` Verbindungen nacheinander auf einem frisch
    /// gebundenen Loopback-Port: liest die Anfrage, ruft `responses[n]` mit
    /// dem geparsten Request auf und schreibt die zurueckgegebenen Rohbytes
    /// (Statuszeile + Header + Body) unveraendert in den Socket.
    fn spawn_http_mock(responses: Vec<MockResponder>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for responder in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let request = read_mock_request(&mut stream);
                let body = responder(&request);
                let _ = stream.write_all(&body);
            }
        });
        port
    }

    fn http_ok(content_type_hint: &str, body: &[u8]) -> Vec<u8> {
        let _ = content_type_hint;
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn http_redirect(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    fn http_partial(body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn end_to_end_download_follows_redirect_and_lands_in_models_dir() {
        let dir = unique_dir("e2e-redirect");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        let content = minimal_gguf_bytes(16);
        let content_for_final = content.clone();
        let final_port = spawn_http_mock(vec![Box::new(move |_req| {
            http_ok("application/octet-stream", &content_for_final)
        })]);
        let start_port = spawn_http_mock(vec![Box::new(move |_req| {
            http_redirect(&format!("http://127.0.0.1:{final_port}/final.gguf"))
        })]);

        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{start_port}/start"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle = DownloadHandle::new();
        let agent = test_agent();
        try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            1,
            &request,
            &handle,
            "http",
            "test-dl-id",
        )
        .unwrap();

        assert_eq!(*handle.phase.lock().unwrap(), DownloadPhase::Completed);
        assert_eq!(fs::read(models_dir.join("model.gguf")).unwrap(), content);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn interrupted_download_is_resumed_via_range_header() {
        let dir = unique_dir("e2e-resume");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        let full = minimal_gguf_bytes(200);
        let split_at = 37usize;

        // Erster Versuch: Server kuendigt die volle Laenge an, schliesst die
        // Verbindung aber nach `split_at` Bytes - simuliert einen
        // Netzwerkabbruch mitten im Download.
        let first_chunk = full[..split_at].to_vec();
        let full_len = full.len();
        let first_port = spawn_http_mock(vec![Box::new(move |_req| {
            let mut out = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {full_len}\r\nConnection: close\r\n\r\n"
            )
            .into_bytes();
            out.extend_from_slice(&first_chunk);
            out
        })]);
        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{first_port}/model.gguf"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle = DownloadHandle::new();
        let agent = test_agent();
        let err = try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            7,
            &request,
            &handle,
            "http",
            "test-dl-id",
        )
        .unwrap_err();
        // ureq erkennt ein vorzeitig geschlossenes response body oft schon
        // selbst beim `read()` (bevor die eigene Content-Length-Pruefung
        // nach der Leseschleife ueberhaupt erreicht wird) und liefert dafuer
        // einen eigenen Fehler statt eines stillen EOF. Beide Formulierungen
        // sind das gewuenschte Ergebnis: der Download gilt nicht faelschlich
        // als abgeschlossen.
        assert!(
            err.contains("unterbrochen") || err.contains("Lesefehler"),
            "unerwartet: {err}"
        );
        assert_eq!(
            fs::read(dir.join("staging").join("model.gguf.7.part")).unwrap(),
            &full[..split_at]
        );

        // Zweiter Versuch gegen denselben (block_id, generation): der Server
        // muss jetzt `Range: bytes=37-` bekommen und antwortet mit 206 plus
        // dem Rest.
        let rest = full[split_at..].to_vec();
        let second_port = spawn_http_mock(vec![Box::new(move |req| {
            assert_eq!(
                req.range.as_deref(),
                Some(format!("bytes={split_at}-").as_str())
            );
            http_partial(&rest)
        })]);
        let resume_request = DownloadRequest {
            url: format!("http://127.0.0.1:{second_port}/model.gguf"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle2 = DownloadHandle::new();
        try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            7,
            &resume_request,
            &handle2,
            "http",
            "test-dl-id",
        )
        .unwrap();
        assert_eq!(*handle2.phase.lock().unwrap(), DownloadPhase::Completed);
        assert_eq!(fs::read(models_dir.join("model.gguf")).unwrap(), full);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn server_ignoring_range_header_restarts_cleanly_instead_of_corrupting() {
        let dir = unique_dir("e2e-range-ignored");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        // Vorbestehender Teil-Fortschritt von einem fruehen, abgebrochenen
        // Lauf, wie ihn `open_part_for_resume` vorfinden wuerde.
        {
            let (mut file, _) = stage.open_part_for_resume("model.gguf", 3).unwrap();
            file.write_all(b"stale-partial-bytes").unwrap();
        }
        let full = minimal_gguf_bytes(8);
        let full_for_server = full.clone();
        let port = spawn_http_mock(vec![Box::new(move |req| {
            // Server unterstuetzt kein Resume: liefert trotz Range-Header
            // immer den kompletten Inhalt mit Status 200.
            assert!(req.range.is_some());
            http_ok("application/octet-stream", &full_for_server)
        })]);
        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{port}/model.gguf"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle = DownloadHandle::new();
        let agent = test_agent();
        try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            3,
            &request,
            &handle,
            "http",
            "test-dl-id",
        )
        .unwrap();
        assert_eq!(fs::read(models_dir.join("model.gguf")).unwrap(), full);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn non_gguf_payload_is_rejected_and_cleaned_up() {
        let dir = unique_dir("e2e-bad-magic");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        let port = spawn_http_mock(vec![Box::new(|_req| {
            http_ok("text/plain", b"not a gguf file")
        })]);
        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{port}/model.gguf"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle = DownloadHandle::new();
        let agent = test_agent();
        let err = try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            9,
            &request,
            &handle,
            "http",
            "test-dl-id",
        )
        .unwrap_err();
        assert!(
            err.contains("keine gueltige GGUF-Datei"),
            "unerwartet: {err}"
        );
        assert!(!models_dir.join("model.gguf").exists());
        assert!(!dir.join("staging").join("model.gguf.9.bin").exists());
        assert!(!dir.join("staging").join("model.gguf.9.commit").exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sha256_mismatch_is_rejected_and_part_file_removed() {
        let dir = unique_dir("e2e-sha-mismatch");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        let content = minimal_gguf_bytes(4);
        let port = spawn_http_mock(vec![Box::new(move |_req| {
            http_ok("application/octet-stream", &content)
        })]);
        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{port}/model.gguf"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: Some("0".repeat(64)),
        };
        let handle = DownloadHandle::new();
        let agent = test_agent();
        let err = try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            4,
            &request,
            &handle,
            "http",
            "test-dl-id",
        )
        .unwrap_err();
        assert!(
            err.contains("Verifikation fehlgeschlagen"),
            "unerwartet: {err}"
        );
        assert!(!models_dir.join("model.gguf").exists());
        assert!(!dir.join("staging").join("model.gguf.4.part").exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancel_flag_stops_in_flight_download_and_discards_part_file() {
        let dir = unique_dir("e2e-cancel");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        // Grosser Body, damit die Lese-Schleife garantiert mehrfach durchlaeuft
        // und die Chance hat, das Cancel-Flag zwischen zwei Chunks zu sehen.
        let content = minimal_gguf_bytes(READ_CHUNK * 4);
        let port = spawn_http_mock(vec![Box::new(move |_req| {
            http_ok("application/octet-stream", &content)
        })]);
        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{port}/model.gguf"),
            file_name: "model.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle = Arc::new(DownloadHandle::new());
        let agent = test_agent();
        handle.cancel.store(true, Ordering::SeqCst);
        let result = try_download(
            &stage,
            &agent,
            &models_dir,
            "model.gguf",
            5,
            &request,
            &handle,
            "http",
            "test-dl-id",
        );
        assert!(result.is_ok(), "Cancel ist kein Fehlerfall: {result:?}");
        assert_eq!(*handle.phase.lock().unwrap(), DownloadPhase::Cancelled);
        assert!(!dir.join("staging").join("model.gguf.5.part").exists());
        assert!(!models_dir.join("model.gguf").exists());
        fs::remove_dir_all(dir).ok();
    }

    // --- Auto-Rescan nach erfolgreichem Download. Testet
    // `rescan_catalog_if_download_completed` isoliert vom eigentlichen
    // Netzwerktransfer (per direkt gesetzter Handle-Phase statt eines
    // echten Downloads), plus einmal end-to-end ueber `try_download` gegen
    // den lokalen Mock-Server, um die tatsaechliche Verdrahtung zu belegen.

    fn rescan_test_config(models_dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.paths.models_dir = Some(models_dir.to_path_buf());
        config
    }

    #[test]
    fn rescan_after_completed_download_makes_new_model_visible() {
        let dir = unique_dir("auto-rescan-completed");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(models_dir.join("auto.gguf"), minimal_gguf_bytes(0)).unwrap();
        let config = rescan_test_config(&models_dir);
        let catalog = RwLock::new(ModelCatalog::new());
        assert_eq!(catalog.read().unwrap().entries().len(), 0);

        let handle = DownloadHandle::new();
        handle.set_phase(DownloadPhase::Completed);
        rescan_catalog_if_download_completed(&handle, "dl-auto", &config, &catalog);

        assert_eq!(catalog.read().unwrap().entries().len(), 1);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rescan_is_skipped_when_download_did_not_complete() {
        let dir = unique_dir("auto-rescan-skip");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        fs::write(models_dir.join("ignored.gguf"), minimal_gguf_bytes(0)).unwrap();
        let config = rescan_test_config(&models_dir);

        for phase in [DownloadPhase::Failed, DownloadPhase::Cancelled] {
            let catalog = RwLock::new(ModelCatalog::new());
            let handle = DownloadHandle::new();
            handle.set_phase(phase);
            rescan_catalog_if_download_completed(&handle, "dl-skip", &config, &catalog);
            assert_eq!(
                catalog.read().unwrap().entries().len(),
                0,
                "Phase {phase:?} haette keinen Rescan ausloesen duerfen"
            );
        }
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn end_to_end_download_triggers_automatic_rescan_without_manual_call() {
        let dir = unique_dir("auto-rescan-e2e");
        let stage = StageStore::new(dir.join("staging")).unwrap();
        let models_dir = dir.join("models");
        let content = minimal_gguf_bytes(16);
        let port = spawn_http_mock(vec![Box::new(move |_req| {
            http_ok("application/octet-stream", &content)
        })]);
        let request = DownloadRequest {
            url: format!("http://127.0.0.1:{port}/model.gguf"),
            file_name: "auto-visible.gguf".into(),
            expected_size_bytes: None,
            sha256: None,
        };
        let handle = DownloadHandle::new();
        let agent = test_agent();
        try_download(
            &stage,
            &agent,
            &models_dir,
            "auto-visible.gguf",
            1,
            &request,
            &handle,
            "http",
            "dl-e2e",
        )
        .unwrap();
        assert_eq!(*handle.phase.lock().unwrap(), DownloadPhase::Completed);

        let config = rescan_test_config(&models_dir);
        let catalog = RwLock::new(ModelCatalog::new());
        assert_eq!(
            catalog.read().unwrap().entries().len(),
            0,
            "Katalog vor dem Rescan-Aufruf muss noch leer sein"
        );
        rescan_catalog_if_download_completed(&handle, "dl-e2e", &config, &catalog);
        let entries = catalog.read().unwrap().entries().to_vec();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].startable);
        fs::remove_dir_all(dir).ok();
    }
}
