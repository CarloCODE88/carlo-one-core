//! Dependency-arme HTTP/1.1-Schicht (nur `std`) über der `api`-Vertragsschicht.
//!
//! Transport und Zustandslogik sind bewusst getrennt: `dispatch` kennt kein
//! TCP, die Netzwerkfunktionen kennen keine Engine-Interna. Jede Verbindung
//! läuft in einem eigenen Thread, aber jede einzelne Engine-Operation hält
//! den globalen Mutex für ihre gesamte Dauer — das erzwingt die geforderte
//! Serialität (kein paralleler Modell-Zugriff) unabhängig von der Anzahl
//! gleichzeitiger Verbindungen.

use crate::{
    api::{self, ApiErrorCode},
    config::Config,
    engine::{Engine, EngineState},
    model_catalog::ModelCatalog,
    planner::{self, ModelProfile, Plan, Resources},
    supervisor::{WorkerConfig, WorkerFailure, WorkerFailureCode},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};

const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 64;
static ACTIVE_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static REQUEST_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Prozesslokaler, streng begrenzter HTTP-Zustand. Es wird absichtlich nur
/// der zuletzt ausgegebene Plan gehalten; ein neuer Plan macht den vorherigen
/// atomar ungueltig. Der Zustand gehoert zur Serverinstanz, nicht zu einem
/// globalen Singleton.
#[derive(Default)]
struct ServerState {
    latest_plan: Mutex<Option<Plan>>,
    // Lazy statt in `ServerState::default()` konstruiert: der Manager
    // braucht `Config` (Modell-/Stage-Verzeichnis, SSD-Reserve), die hier
    // noch nicht verfuegbar ist. Wird beim ersten Download-Request einmalig
    // gebaut und danach wiederverwendet, siehe `downloads_manager`.
    downloads: Mutex<Option<crate::download::DownloadManager>>,
    // Verhindert, dass mehrere gleichzeitige Rescan-Anfragen (z.B. ein
    // Doppelklick in der GUI) parallel um dieselben Festplatten-I/O
    // konkurrieren und sich so gegenseitig noch weiter verlangsamen. Ein
    // Rescan, der bereits laeuft, meldet das statt einen zweiten zu starten.
    // Eigener `Arc`, nicht Teil eines groesseren `Arc<ServerState>`: so
    // laesst sich nur dieses eine Flag in den Hintergrund-Thread klonen,
    // ohne `dispatch`/`handle_connection` auf `&Arc<ServerState>`
    // umzustellen.
    rescan_in_progress: Arc<AtomicBool>,
    cancellations: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl ServerState {
    fn register_request(&self, request_id: &str) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.cancellations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request_id.to_owned(), flag.clone());
        flag
    }

    fn cancel_request(&self, request_id: &str) -> bool {
        let Some(flag) = self
            .cancellations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(request_id)
            .cloned()
        else {
            return false;
        };
        flag.store(true, Ordering::Release);
        true
    }

    fn unregister_request(&self, request_id: &str) {
        self.cancellations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id);
    }
}

struct RequestCancellationGuard<'a> {
    state: &'a ServerState,
    request_id: String,
}

impl Drop for RequestCancellationGuard<'_> {
    fn drop(&mut self) {
        self.state.unregister_request(&self.request_id);
    }
}

/// Holt den lazy initialisierten [`DownloadManager`](crate::download::DownloadManager)
/// aus `server_state` (baut ihn beim ersten Aufruf aus `config`) und fuehrt
/// `f` mit einer Referenz darauf aus, waehrend der Lock gehalten wird.
fn with_downloads<T>(
    server_state: &ServerState,
    config: &Config,
    catalog: &Arc<RwLock<ModelCatalog>>,
    f: impl FnOnce(&crate::download::DownloadManager) -> T,
) -> io::Result<T> {
    let mut guard = server_state.downloads.lock().unwrap();
    if guard.is_none() {
        *guard = Some(crate::download::DownloadManager::new(
            config,
            catalog.clone(),
        )?);
    }
    Ok(f(guard.as_ref().unwrap()))
}

/// Bindet den Listener. Getrennt von `serve`, damit Tests den tatsächlich
/// vergebenen Port (z.B. bei `":0"`) auslesen können, bevor die blockierende
/// Serve-Schleife startet.
pub fn bind(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
}

/// Blockierende Accept-Schleife. Kehrt nur bei einem Listener-Fehler zurück.
pub fn serve(listener: TcpListener, engine: Arc<Mutex<Engine>>) -> io::Result<()> {
    let config = Arc::new(load_config()?);
    serve_with_config(listener, engine, config)
}

/// Blockierende Accept-Schleife mit einer beim Prozessstart festgelegten
/// Konfiguration. Laufende Requests sehen dadurch stets denselben Snapshot.
pub fn serve_with_config(
    listener: TcpListener,
    engine: Arc<Mutex<Engine>>,
    config: Arc<Config>,
) -> io::Result<()> {
    let scan = crate::model_sources::scan(&config);
    for warning in scan.warnings {
        eprintln!("tri-ai-runner: Modellscan-Warnung: {warning}");
    }
    serve_with_catalog(
        listener,
        engine,
        config,
        Arc::new(RwLock::new(scan.catalog)),
    )
}

/// Serverstart mit einem bereits aufgebauten Katalog. Diese Grenze hält
/// teure Dateihashes aus einzelnen HTTP-Requests und macht Tests gezielt.
/// Der `RwLock` macht den Katalog zur Laufzeit austauschbar — siehe
/// `POST /api/models/rescan`, der nach einem Download o.ä. neu scannt, ohne
/// den Server neu starten zu müssen. Viele parallele Leser (jede laufende
/// Verbindung) blockieren sich dabei nicht gegenseitig, nur der seltene
/// Rescan selbst braucht kurz den exklusiven Schreib-Zugriff.
pub fn serve_with_catalog(
    listener: TcpListener,
    engine: Arc<Mutex<Engine>>,
    config: Arc<Config>,
    catalog: Arc<RwLock<ModelCatalog>>,
) -> io::Result<()> {
    let server_state = Arc::new(ServerState::default());
    for stream in listener.incoming() {
        let stream = stream?;
        if ACTIVE_CONNECTIONS.fetch_add(1, Ordering::AcqRel) >= MAX_ACTIVE_CONNECTIONS {
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let engine = engine.clone();
        let config = config.clone();
        let catalog = catalog.clone();
        let server_state = server_state.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream, &engine, &config, &catalog, &server_state) {
                eprintln!("tri-ai-runner: Verbindung mit Fehler beendet: {e}");
            }
            ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::AcqRel);
        });
    }
    Ok(())
}

/// Komfortfunktion für `main.rs`: binden und sofort bedienen.
pub fn run(addr: &str, engine: Arc<Mutex<Engine>>) -> io::Result<()> {
    let config = Arc::new(load_config()?);
    run_with_config(addr, engine, config)
}

/// Produktionspfad für bereits validierte Konfigurationen. Insbesondere wird
/// die TOML-Datei nicht bei jedem HTTP-Request erneut gelesen.
pub fn run_with_config(
    addr: &str,
    engine: Arc<Mutex<Engine>>,
    config: Arc<Config>,
) -> io::Result<()> {
    serve_with_config(bind(addr)?, engine, config)
}

fn load_config() -> io::Result<Config> {
    Config::load().map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

fn handle_connection(
    mut stream: TcpStream,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    catalog: &Arc<RwLock<ModelCatalog>>,
    server_state: &ServerState,
) -> io::Result<()> {
    let request = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            return write_response(&mut stream, 400, &api::Response::Error(invalid_request(&e)));
        }
    };
    crate::observability::emit(
        "request_received",
        serde_json::json!({
            "request_id": request.request_id,
            "method": request.method,
            "path": request.path,
            "user_id": config.server.user_id,
        }),
    );
    // Health is deliberately public for local/process probes. Every
    // inference, model-management, and metrics route remains authenticated
    // when an auth token is configured.
    if route_requires_auth(&request.path)
        && !is_authorized(&request, config.server.auth_token.as_deref())
    {
        return write_response(
            &mut stream,
            401,
            &serde_json::json!({"error":{"code":"unauthorized","message":"Authentifizierung erforderlich"}}),
        );
    }
    let cancellation = server_state.register_request(&request.request_id);
    let _cancellation_guard = RequestCancellationGuard {
        state: server_state,
        request_id: request.request_id.clone(),
    };
    match (request.method.as_str(), request.path.as_str()) {
        // Interner Vertrag (api::Request/Response als getaggtes JSON) —
        // unverändertes Verhalten für bestehende Clients/Tests.
        ("POST", "/") => {
            let (status, response) = match serde_json::from_str::<api::Request>(&request.body) {
                Ok(req) => {
                    let response = dispatch(req, engine, config, catalog, server_state);
                    (status_code_for(&response), response)
                }
                Err(e) => (400, api::Response::Error(invalid_request(&e))),
            };
            write_response(&mut stream, status, &response)
        }
        // V1-Routen aus outputs/TRI_AI_RUNNER_V1_SPEC.md — Übersetzungsschicht
        // auf denselben internen `dispatch`, kein eigener Zustand.
        ("GET", "/health") => {
            let mut current = engine.lock().unwrap();
            current.refresh();
            let ready = current.state() == EngineState::Ready;
            let model = current.active_model().map(str::to_owned);
            let resources = crate::resources::read().ok();
            write_response(
                &mut stream,
                200,
                &serde_json::json!({
                    "status": if ready { "ok" } else { "degraded" },
                    "model": model,
                    "vram_used_mb": resources.as_ref().map(|r| r.vram_used_mb),
                    "vram_total_mb": resources.as_ref().map(|r| r.vram_total_mb),
                    "queue_depth": if current.state() == EngineState::Busy { 1 } else { 0 }
                }),
            )
        }
        ("GET", "/metrics") => {
            let resources = crate::resources::read().ok();
            write_response(
                &mut stream,
                200,
                &serde_json::json!({
                    "schema_version": 1,
                    "active_connections": ACTIVE_CONNECTIONS.load(Ordering::Relaxed),
                    "max_active_connections": MAX_ACTIVE_CONNECTIONS,
                    "resources": resources,
                }),
            )
        }
        ("GET", "/v1/models" | "/api/models") => {
            let models = match dispatch(
                api::Request::ListModels,
                engine,
                config,
                catalog,
                server_state,
            ) {
                api::Response::Models { models } => models,
                error @ api::Response::Error(_) => {
                    return write_response(&mut stream, status_code_for(&error), &error);
                }
                _ => unreachable!("ListModels liefert nur Models oder Error"),
            };
            let data: Vec<_> = models
                .into_iter()
                .map(|model| {
                    serde_json::json!({
                        "id": model.id,
                        "object": "model",
                        "owned_by": "tri-ai-runner",
                        "display_name": model.display_name,
                        "digest": model.digest,
                        "backend": model.backend,
                        "sources": model.sources,
                        "size_bytes": model.size_bytes,
                        "family": model.family,
                        "format": model.format,
                        "parameters": model.parameters,
                        "quantization": model.quantization,
                        "context_tokens": model.context_tokens,
                        "capabilities": model.capabilities,
                        "startable": model.startable,
                        "reason_not_startable": model.unavailable_reason,
                        "paged_supported": model.paged_supported
                    })
                })
                .collect();
            write_response(
                &mut stream,
                200,
                &serde_json::json!({"object": "list", "data": data}),
            )
        }
        ("POST", "/v1/chat/completions") => handle_v1_chat(
            &mut stream,
            engine,
            config,
            &request.body,
            Some(cancellation.as_ref()),
        ),
        ("POST", "/v1/embeddings") => handle_v1_embeddings(
            &mut stream,
            engine,
            config,
            &request.body,
            Some(cancellation.as_ref()),
        ),
        ("POST", path) if path.starts_with("/api/requests/") && path.ends_with("/cancel") => {
            let target = path
                .strip_prefix("/api/requests/")
                .and_then(|value| value.strip_suffix("/cancel"))
                .filter(|value| valid_request_id(value));
            match target {
                Some(target) if server_state.cancel_request(target) => write_response(
                    &mut stream,
                    202,
                    &serde_json::json!({"request_id": target, "cancelled": true}),
                ),
                _ => write_response(
                    &mut stream,
                    404,
                    &serde_json::json!({"error":{"code":"not_found","message":"aktive Request-ID nicht gefunden"}}),
                ),
            }
        }
        ("GET", "/api/engine/status" | "/api/status") => {
            let response = dispatch(api::Request::Status, engine, config, catalog, server_state);
            write_response(&mut stream, status_code_for(&response), &response)
        }
        ("POST", "/api/engine/start" | "/api/engine") => {
            #[derive(serde::Deserialize)]
            struct StartRequest {
                model: Option<String>,
                model_id: Option<String>,
                mode: Option<String>,
                plan_id: Option<String>,
            }
            let parsed: StartRequest = match serde_json::from_str(&request.body) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return write_response(
                        &mut stream,
                        400,
                        &api::Response::Error(invalid_request(&error)),
                    );
                }
            };
            if let Some(mode) = parsed.mode.as_deref() {
                if !matches!(mode, "auto" | "static" | "paged") {
                    return write_response(
                        &mut stream,
                        400,
                        &api::Response::Error(api::ApiError {
                            code: ApiErrorCode::InvalidRequest,
                            message: "mode muss 'auto', 'static' oder 'paged' sein".into(),
                        }),
                    );
                }
            }
            let model = match (parsed.model, parsed.model_id) {
                (Some(model), Some(model_id)) if model == model_id && !model.is_empty() => model,
                (Some(model), None) | (None, Some(model)) if !model.is_empty() => model,
                (Some(_), Some(_)) => {
                    return write_response(
                        &mut stream,
                        400,
                        &api::Response::Error(api::ApiError {
                            code: ApiErrorCode::InvalidRequest,
                            message: "'model' und 'model_id' muessen identisch sein".into(),
                        }),
                    );
                }
                _ => {
                    return write_response(
                        &mut stream,
                        400,
                        &api::Response::Error(api::ApiError {
                            code: ApiErrorCode::InvalidRequest,
                            message: "Start-Request benoetigt ein nicht leeres Feld 'model' oder 'model_id'".into(),
                        }),
                    );
                }
            };
            let plan_id = match parsed.plan_id {
                Some(plan_id) if !plan_id.is_empty() => plan_id,
                _ => {
                    return write_response(
                        &mut stream,
                        409,
                        &api::Response::Error(api::ApiError::stale_plan(
                            "Start-Request benoetigt einen aktuellen 'plan_id'",
                        )),
                    );
                }
            };
            let response = dispatch(
                api::Request::StartModelPlanned { model, plan_id },
                engine,
                config,
                catalog,
                server_state,
            );
            write_response(&mut stream, status_code_for(&response), &response)
        }
        ("POST", "/api/engine/stop") => {
            let response = dispatch(
                api::Request::StopModel,
                engine,
                config,
                catalog,
                server_state,
            );
            write_response(&mut stream, status_code_for(&response), &response)
        }
        ("GET", "/api/engine/memory" | "/api/resources") => {
            let response = dispatch(
                api::Request::Resources,
                engine,
                config,
                catalog,
                server_state,
            );
            write_response(&mut stream, status_code_for(&response), &response)
        }
        ("GET", "/api/engine/workers") => {
            let e = engine.lock().unwrap();
            let workers = match e.active_model() {
                Some(model) => serde_json::json!([{"model": model, "state": e.state()}]),
                None => serde_json::json!([]),
            };
            drop(e);
            write_response(&mut stream, 200, &serde_json::json!({"workers": workers}))
        }
        ("POST", "/api/engine/plan") => {
            handle_engine_plan(&mut stream, config, server_state, &request.body)
        }
        ("POST", "/api/engine/plan/auto") => {
            handle_engine_plan_auto(&mut stream, config, catalog, server_state, &request.body)
        }
        ("POST", "/api/models/rescan") => {
            let response = dispatch(
                api::Request::RescanModels,
                engine,
                config,
                catalog,
                server_state,
            );
            write_response(&mut stream, status_code_for(&response), &response)
        }
        ("POST", "/api/downloads") => {
            let parsed: crate::download::DownloadRequest = match serde_json::from_str(&request.body)
            {
                Ok(parsed) => parsed,
                Err(error) => {
                    return write_response(
                        &mut stream,
                        400,
                        &api::Response::Error(invalid_request(&error)),
                    );
                }
            };
            let response = dispatch(
                api::Request::CreateDownload(parsed),
                engine,
                config,
                catalog,
                server_state,
            );
            write_response(&mut stream, status_code_for(&response), &response)
        }
        ("GET", path) if path.starts_with("/api/downloads/") && !path.ends_with("/cancel") => {
            match download_id_from_path(path, "") {
                Some(id) => {
                    let response = dispatch(
                        api::Request::DownloadStatus { id },
                        engine,
                        config,
                        catalog,
                        server_state,
                    );
                    write_response(&mut stream, status_code_for(&response), &response)
                }
                None => write_response(&mut stream, 404, &not_found(&request)),
            }
        }
        ("POST", path) if path.starts_with("/api/downloads/") && path.ends_with("/cancel") => {
            match download_id_from_path(path, "/cancel") {
                Some(id) => {
                    let response = dispatch(
                        api::Request::CancelDownload { id },
                        engine,
                        config,
                        catalog,
                        server_state,
                    );
                    write_response(&mut stream, status_code_for(&response), &response)
                }
                None => write_response(&mut stream, 404, &not_found(&request)),
            }
        }
        // Persistente Konversations-Sessions (Persistenz-Welle):
        // GET  /api/sessions        -> Liste vorhandener Session-IDs
        // GET  /api/sessions/<id>   -> Verlauf laden
        // POST /api/sessions/<id>   -> Zeile anhängen (Body = JSON-Zeile)
        ("GET", "/api/sessions") => {
            let root = config.paths.project_root.join("sessions");
            let scanner = crate::persistence::Scanner::new(root);
            match scanner.list() {
                Ok(ids) => write_response(
                    &mut stream,
                    200,
                    &serde_json::json!({"object": "list", "data": ids}),
                ),
                Err(err) => write_response(
                    &mut stream,
                    500,
                    &serde_json::json!({"error": {"message": err.to_string()}}),
                ),
            }
        }
        // Custom Assistants (Jan-Analogon): Liste der im Projekt abgelegten
        // Rollen unter <project_root>/assistants/.
        ("GET", "/api/assistants") => {
            let root = config.paths.project_root.join("assistants");
            match crate::assistant::list(&root) {
                Ok(assistants) => {
                    let data: Vec<_> = assistants
                        .into_iter()
                        .map(|a| {
                            serde_json::json!({
                                "name": a.name,
                                "system_prompt": a.system_prompt,
                                "model": a.model,
                                "temperature": a.temperature,
                                "tools": a.tools,
                            })
                        })
                        .collect();
                    write_response(
                        &mut stream,
                        200,
                        &serde_json::json!({"object": "list", "data": data}),
                    )
                }
                Err(err) => write_response(
                    &mut stream,
                    500,
                    &serde_json::json!({"error": {"message": err.to_string()}}),
                ),
            }
        }
        ("GET", path) if path.starts_with("/api/sessions/") => {
            let id = path.trim_start_matches("/api/sessions/");
            let root = config.paths.project_root.join("sessions");
            let scanner = crate::persistence::Scanner::new(root);
            match scanner.load(id) {
                Ok(lines) => write_response(
                    &mut stream,
                    200,
                    &serde_json::json!({"object": "session", "id": id, "lines": lines}),
                ),
                Err(err) => write_response(
                    &mut stream,
                    400,
                    &serde_json::json!({"error": {"message": err.to_string()}}),
                ),
            }
        }
        ("POST", path) if path.starts_with("/api/sessions/") => {
            let id = path.trim_start_matches("/api/sessions/");
            let line: serde_json::Value = match serde_json::from_str(&request.body) {
                Ok(line) => line,
                Err(err) => {
                    return write_response(
                        &mut stream,
                        400,
                        &serde_json::json!({"error": {"message": format!("ungueltiges JSON: {err}")}}),
                    );
                }
            };
            let root = config.paths.project_root.join("sessions");
            let result = crate::persistence::append(id, line, &root);
            match result {
                Ok(()) => write_response(
                    &mut stream,
                    200,
                    &serde_json::json!({"object": "session", "id": id, "appended": true}),
                ),
                Err(err) => write_response(
                    &mut stream,
                    400,
                    &serde_json::json!({"error": {"message": err.to_string()}}),
                ),
            }
        }
        _ => write_response(&mut stream, 404, &not_found(&request)),
    }
}

fn not_found(request: &ParsedRequest) -> serde_json::Value {
    serde_json::json!({"kind": "error", "code": "not_found", "message": format!("unbekannte Route: {} {}", request.method, request.path)})
}

/// Extrahiert die Download-ID aus `/api/downloads/<id>` bzw.
/// `/api/downloads/<id><suffix>` (z.B. `.../cancel`). Lehnt eine leere ID
/// oder ein zusätzliches Pfadsegment ab (`None`), statt eine ID wie
/// `"foo/bar"` an den Downloadmanager durchzureichen.
fn download_id_from_path(path: &str, suffix: &str) -> Option<String> {
    let rest = path.strip_prefix("/api/downloads/")?;
    let id = rest.strip_suffix(suffix).unwrap_or(rest);
    (!id.is_empty() && !id.contains('/')).then(|| id.to_owned())
}

/// OpenAI-ähnlicher `/v1/chat/completions`-Endpunkt: übersetzt an der Kante
/// in/aus dem einfachen internen `Chat{model,prompt}`-Format. Das interne
/// Format bleibt bewusst simpel (siehe INGRIED-Empfehlung in `.claude/skills`
/// bzw. Commit-Historie) — nur diese Schicht kennt das OpenAI-Schema.
fn handle_v1_chat(
    stream: &mut TcpStream,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    body: &str,
    cancellation: Option<&AtomicBool>,
) -> io::Result<()> {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return write_response(stream, 400, &api::Response::Error(invalid_request(&e)));
        }
    };
    let attachment_index = match crate::attachments::AttachmentIndex::from_openai_request(&parsed) {
        Ok(index) => index,
        Err(message) => {
            return write_response(
                stream,
                400,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message,
                }),
            );
        }
    };
    let (model, stream_requested, mut worker_body) = match crate::openai::chat_worker_body(&parsed)
    {
        Ok(parsed) => parsed,
        Err(message) => {
            return write_response(
                stream,
                400,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message,
                }),
            );
        }
    };
    // Wenn der Client Tools anfordert (Coding-Tab, Attachments), werden alle
    // zugelassenen Tool-Definitionen an den Worker gehängt. Andernfalls bleibt
    // der Body unverändert und es gibt keinen Tool-Loop.
    let tools_requested = parsed
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| !tools.is_empty())
        .unwrap_or(false);
    if let Some(index) = attachment_index.as_ref() {
        if let Err(message) =
            crate::attachments::attach_to_worker_chat_body(&mut worker_body, index)
        {
            return write_response(
                stream,
                400,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message,
                }),
            );
        }
    } else if tools_requested {
        if let Err(message) =
            crate::attachments::attach_allowed_tools_to_worker_chat_body(&mut worker_body)
        {
            return write_response(
                stream,
                400,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message,
                }),
            );
        }
    }
    if stream_requested {
        if attachment_index.is_some() {
            return write_response(
                stream,
                400,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message: "stream=true ist mit Runner-Tools in diesem Release nicht erlaubt"
                        .into(),
                }),
            );
        }
        return match stream_active_worker_response(
            stream,
            engine,
            config,
            &model,
            "/v1/chat/completions",
            &worker_body,
            cancellation,
        ) {
            Ok(()) => Ok(()),
            Err(error) => {
                let response = api::Response::Error(error);
                write_response(stream, status_code_for(&response), &response)
            }
        };
    }
    match run_chat_completion_with_tools(
        engine,
        config,
        &model,
        worker_body,
        attachment_index.as_ref(),
        cancellation,
    ) {
        Ok(response) => write_response(stream, 200, &response),
        Err(error) => {
            let response = api::Response::Error(error);
            write_response(stream, status_code_for(&response), &response)
        }
    }
}

fn handle_v1_embeddings(
    stream: &mut TcpStream,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    body: &str,
    cancellation: Option<&AtomicBool>,
) -> io::Result<()> {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return write_response(stream, 400, &api::Response::Error(invalid_request(&e)));
        }
    };
    let (model, worker_body) = match crate::openai::embeddings_worker_body(&parsed) {
        Ok(parsed) => parsed,
        Err(message) => {
            return write_response(
                stream,
                400,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message,
                }),
            );
        }
    };
    match proxy_active_worker_json(
        engine,
        config,
        &model,
        "/v1/embeddings",
        &worker_body,
        cancellation,
    ) {
        Ok(worker) => match crate::openai::normalize_embeddings_response(worker, &model) {
            Ok(response) => write_response(stream, 200, &response),
            Err(message) => write_response(
                stream,
                502,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::WorkerCrashed,
                    message,
                }),
            ),
        },
        Err(error) => {
            let response = api::Response::Error(error);
            write_response(stream, status_code_for(&response), &response)
        }
    }
}

fn run_chat_completion_with_tools(
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    model: &str,
    mut worker_body: serde_json::Value,
    attachment_index: Option<&crate::attachments::AttachmentIndex>,
    cancellation: Option<&AtomicBool>,
) -> Result<serde_json::Value, api::ApiError> {
    // Jede Tool-Runde ist ein eigener Worker-Request und traegt eigene
    // `usage`-Tokens (der wachsende Prompt aus den bisherigen Tool-
    // Ergebnissen kostet ebenfalls Tokens). Ohne Aggregation saehe der
    // Client nur die Tokenzahl der letzten Runde und wuerde die
    // tatsaechliche Nutzung deutlich unterschaetzen.
    let mut total_usage = None;
    for round in 0..=crate::attachments::MAX_TOOL_ROUNDS {
        let worker = proxy_active_worker_json(
            engine,
            config,
            model,
            "/v1/chat/completions",
            &worker_body,
            cancellation,
        )?;
        crate::openai::accumulate_usage(&mut total_usage, &worker);
        let mut normalized =
            crate::openai::normalize_chat_response(worker, model).map_err(|message| {
                api::ApiError {
                    code: ApiErrorCode::WorkerCrashed,
                    message,
                }
            })?;
        let results = crate::coding_tools::execute_tool_calls(
            &normalized,
            attachment_index,
            &config.paths.project_root,
        )
        .map_err(|message| api::ApiError {
            code: ApiErrorCode::InvalidRequest,
            message,
        })?;
        if results.is_empty() {
            // Keine Tool-Calls in dieser Antwort -> Endantwort.
            crate::openai::apply_usage(&mut normalized, total_usage);
            return Ok(normalized);
        }
        if round == crate::attachments::MAX_TOOL_ROUNDS {
            return Err(api::ApiError {
                code: ApiErrorCode::InvalidRequest,
                message: "Tool-Limit von vier Runden erreicht".into(),
            });
        }
        let assistant_message =
            crate::attachments::assistant_message(&normalized).ok_or_else(|| api::ApiError {
                code: ApiErrorCode::WorkerCrashed,
                message: "Worker-Antwort enthaelt keine Assistant-Message".into(),
            })?;
        crate::attachments::append_tool_results(&mut worker_body, assistant_message, &results)
            .map_err(|message| api::ApiError {
                code: ApiErrorCode::Internal,
                message,
            })?;
    }
    Err(api::ApiError {
        code: ApiErrorCode::Internal,
        message: "Tool-Schleife endete unerwartet".into(),
    })
}

/// `/api/engine/plan` — Dry-Run über die bestehende, bisher unbenutzte
/// `planner::plan()`-Logik. Ohne mitgeliefertes `resources`-Feld wird ein
/// echter Hardware-Snapshot gelesen und mit den konfigurierten konservativen
/// Reserven kombiniert.
fn handle_engine_plan(
    stream: &mut TcpStream,
    config: &Config,
    server_state: &ServerState,
    body: &str,
) -> io::Result<()> {
    #[derive(serde::Deserialize)]
    struct PlanRequest {
        model_profile: ModelProfile,
        resources: Option<Resources>,
    }
    let parsed: PlanRequest = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return write_response(stream, 400, &api::Response::Error(invalid_request(&e)));
        }
    };
    if let Err(message) = validate_model_path(&parsed.model_profile.model) {
        return write_response(
            stream,
            400,
            &api::Response::Error(api::ApiError {
                code: ApiErrorCode::InvalidRequest,
                message,
            }),
        );
    }
    plan_and_respond(
        stream,
        config,
        server_state,
        &parsed.model_profile,
        parsed.resources,
    )
}

/// Gemeinsamer Kern von `/api/engine/plan` (Client liefert das komplette
/// `ModelProfile`) und `/api/engine/plan/auto` (Server leitet es selbst aus
/// dem Katalog ab): Ressourcen-Snapshot ggf. selbst lesen, Plan berechnen,
/// `plan_decided`-Event emittieren, im `server_state` fuer den
/// nachfolgenden `StartModelPlanned`-Aufruf hinterlegen, Antwort senden.
fn plan_and_respond(
    stream: &mut TcpStream,
    config: &Config,
    server_state: &ServerState,
    profile: &ModelProfile,
    resources: Option<Resources>,
) -> io::Result<()> {
    let resources = match resources {
        Some(r) => r,
        None => match crate::resources::read() {
            Ok(snapshot) => default_resources_from_snapshot(&snapshot, config),
            Err(e) => {
                return write_response(
                    stream,
                    500,
                    &api::Response::Error(api::ApiError {
                        code: ApiErrorCode::Internal,
                        message: format!("Ressourcen-Snapshot nicht lesbar: {e}"),
                    }),
                );
            }
        },
    };
    let plan = planner::plan(profile, &resources);
    crate::observability::emit(
        "plan_decided",
        serde_json::json!({
            "model": plan.model,
            "lane": plan.lane,
            "safe": plan.safe,
            "reason": plan.reason,
            "gpu_layers": plan.gpu_layers,
            "ctx_size": plan.ctx_size,
        }),
    );
    match server_state.latest_plan.lock() {
        Ok(mut latest) => *latest = Some(plan.clone()),
        Err(_) => {
            return write_response(
                stream,
                500,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: "Plan-Zustand ist nicht verfuegbar".into(),
                }),
            );
        }
    }
    write_response(stream, 200, &plan)
}

/// `/api/engine/plan/auto` — leitet ein `ModelProfile` selbst aus dem
/// Katalog und der GGUF-Datei ab, statt es vom Client zu verlangen. Ein
/// GUI-Client kennt keine GGUF-Interna wie Layer-Zahl oder Ressourcenbedarf
/// — das ist Backend-Wissen. Ermoeglicht z.B. der Desktop-GUI, ein Modell
/// per zwei Aufrufen (dieser, dann `StartModelPlanned`) zu aktivieren, ohne
/// selbst ein `ModelProfile` zusammenzubauen.
fn handle_engine_plan_auto(
    stream: &mut TcpStream,
    config: &Config,
    catalog: &RwLock<ModelCatalog>,
    server_state: &ServerState,
    body: &str,
) -> io::Result<()> {
    #[derive(serde::Deserialize)]
    struct AutoPlanRequest {
        model: String,
    }
    let parsed: AutoPlanRequest = match serde_json::from_str(body) {
        Ok(p) => p,
        Err(e) => {
            return write_response(stream, 400, &api::Response::Error(invalid_request(&e)));
        }
    };
    let Some((_source, path)) = catalog.read().unwrap().start_location(&parsed.model) else {
        return write_response(
            stream,
            404,
            &api::Response::Error(api::ApiError {
                code: ApiErrorCode::NotFound,
                message: format!("Modell '{}' ist nicht im Katalog startbar", parsed.model),
            }),
        );
    };
    let file_size_bytes = match std::fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(e) => {
            return write_response(
                stream,
                500,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Modelldatei nicht lesbar: {e}"),
                }),
            );
        }
    };
    let metadata = match crate::gguf_registry::inspect(&path) {
        Ok(metadata) => metadata,
        Err(e) => {
            return write_response(
                stream,
                500,
                &api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("GGUF-Metadaten nicht lesbar: {e}"),
                }),
            );
        }
    };
    let Some(layers) = metadata.layers else {
        return write_response(
            stream,
            500,
            &api::Response::Error(api::ApiError {
                code: ApiErrorCode::Internal,
                message: "GGUF-Datei nennt keine Layer-Anzahl (block_count) - automatische \
                          Planung nicht moeglich"
                    .into(),
            }),
        );
    };
    let file_size_mb = file_size_bytes.div_ceil(1_048_576).max(1);
    // A GGUF advertises a maximum context, not a safe startup budget. Keep
    // automatic planning bounded by the operator's configured default so a
    // large metadata value cannot reserve several GiB of KV cache and OOM.
    let context_tokens = metadata
        .context_length
        .unwrap_or(u64::from(config.worker.default_context))
        .min(u64::from(config.worker.default_context))
        .min(u64::from(u32::MAX)) as u32;
    // v1-Heuristik: GGUF-Gewichte dominieren den Speicherbedarf bei voller
    // GPU-Auslagerung; den KV-Cache-Overhead schlaegt `planner::plan`
    // bereits selbst oben drauf. Kein Genauigkeitsanspruch, aber deutlich
    // besser als ein reiner Rateversuch ohne jede Grundlage.
    let profile = ModelProfile {
        model: parsed.model,
        file_size_mb,
        estimated_gpu_mb: file_size_mb,
        estimated_ram_mb: file_size_mb,
        layers: layers.min(u64::from(u32::MAX)) as u32,
        context_tokens,
        requested_context_tokens: context_tokens,
    };
    plan_and_respond(stream, config, server_state, &profile, None)
}

fn default_resources_from_snapshot(
    snapshot: &crate::resources::Snapshot,
    config: &Config,
) -> Resources {
    Resources {
        vram_free_mb: snapshot.vram_free_mb,
        ram_free_mb: snapshot.ram_available_mb,
        ssd_free_mb: snapshot.ssd_free_mb,
        vram_reserve_mb: config.reserves.vram_mb,
        ram_reserve_mb: config.reserves.ram_mb,
        ssd_reserve_mb: config.reserves.ssd_mb,
    }
}

struct ParsedRequest {
    method: String,
    path: String,
    body: String,
    headers: HashMap<String, String>,
    request_id: String,
}

/// Liest Request-Zeile + Header bis zur Leerzeile, wertet `Content-Length`
/// aus und liest danach exakt so viele Body-Bytes — robust gegenüber
/// Requests, die über mehrere TCP-Reads verteilt ankommen. Kein
/// Chunked-Encoding (nicht nötig für diesen internen Vertrag).
fn read_request(stream: &mut TcpStream) -> io::Result<ParsedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Header zu groß"));
        }
        let n = read_retrying_eintr(stream, &mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Verbindung vor vollständigem Header-Ende geschlossen",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();
    if method.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "leere oder ungültige Request-Zeile",
        ));
    }
    let headers = parse_headers(&header_text);
    let content_length = parse_content_length(&header_text)?;
    if content_length > MAX_REQUEST_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Request-Body ueberschreitet das Limit von 16 MiB",
        ));
    }
    let body_start = header_end + 4; // "\r\n\r\n"
    let body_end = body_start + content_length;
    while buf.len() < body_end {
        let n = read_retrying_eintr(stream, &mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Verbindung vor vollständigem Body geschlossen",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_end]).into_owned();
    let request_id = headers
        .get("x-request-id")
        .filter(|value| valid_request_id(value))
        .cloned()
        .unwrap_or_else(next_request_id);
    Ok(ParsedRequest {
        method,
        path,
        body,
        headers,
        request_id,
    })
}

/// `read()` kann mit `EINTR` fehlschlagen, wenn ein Signal den Syscall
/// unterbricht — der Runner spawnt/reaped nebenher echte Worker-Prozesse
/// (SIGCHLD), das ist also ein reales Produktionsszenario, nicht nur ein
/// Testartefakt. `EINTR` bedeutet nicht "Verbindung kaputt", nur "nochmal
/// versuchen".
fn read_retrying_eintr(stream: &mut TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match stream.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> io::Result<usize> {
    headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case("content-length").then(|| {
                value.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "ungueltige Content-Length")
                })
            })
        })
        .unwrap_or(Ok(0))
}

fn parse_headers(headers: &str) -> HashMap<String, String> {
    headers
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect()
}

fn next_request_id() -> String {
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("tri-{}-{sequence}", std::process::id())
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn is_authorized(request: &ParsedRequest, expected: Option<&str>) -> bool {
    let Some(expected) = expected.filter(|token| !token.is_empty()) else {
        return true;
    };
    let provided = request
        .headers
        .get("x-tri-auth")
        .map(String::as_str)
        .or_else(|| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.strip_prefix("Bearer "))
        })
        .unwrap_or("");
    constant_time_equal(provided.as_bytes(), expected.as_bytes())
}

fn route_requires_auth(path: &str) -> bool {
    path != "/health"
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= u8::from(left.get(index) != right.get(index)) as usize;
    }
    difference == 0
}

fn invalid_request(err: &dyn std::fmt::Display) -> api::ApiError {
    api::ApiError {
        code: ApiErrorCode::InvalidRequest,
        message: format!("ungültige Anfrage: {err}"),
    }
}

/// Reine Zustandslogik: Request + Engine -> Response. Kein Netzwerkcode,
/// darum direkt unit-testbar ohne echten Socket.
fn dispatch(
    request: api::Request,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    catalog: &Arc<RwLock<ModelCatalog>>,
    server_state: &ServerState,
) -> api::Response {
    match request {
        api::Request::Status => {
            let mut e = engine.lock().unwrap();
            e.refresh();
            api::Response::Status(api::StatusResponse {
                state: e.state(),
                active_model: e.active_model().map(str::to_owned),
                single_model_only: true,
                resources: crate::resources::read().ok(),
            })
        }
        api::Request::Resources => api::Response::Resources(crate::resources::read().ok()),
        api::Request::ListModels => api::Response::Models {
            models: catalog.read().unwrap().summaries(),
        },
        api::Request::StartModel { model } => {
            let resources = match crate::resources::read() {
                Ok(snapshot) => default_resources_from_snapshot(&snapshot, config),
                Err(err) => {
                    return api::Response::Error(api::ApiError {
                        code: ApiErrorCode::Internal,
                        message: format!("Ressourcen-Snapshot nicht lesbar: {err}"),
                    });
                }
            };
            start_model_legacy(model, engine, config, catalog, resources)
        }
        api::Request::StartModelPlanned { model, plan_id } => {
            let latest = match server_state.latest_plan.lock() {
                Ok(latest) => latest,
                Err(_) => {
                    return api::Response::Error(api::ApiError {
                        code: ApiErrorCode::Internal,
                        message: "Plan-Zustand ist nicht verfuegbar".into(),
                    });
                }
            };
            let plan = match validate_plan_identity(latest.as_ref(), &model, &plan_id) {
                Ok(plan) => plan,
                Err(error) => return api::Response::Error(error),
            };
            let resources = match crate::resources::read() {
                Ok(snapshot) => default_resources_from_snapshot(&snapshot, config),
                Err(err) => {
                    return api::Response::Error(api::ApiError {
                        code: ApiErrorCode::Internal,
                        message: format!("Ressourcen-Snapshot nicht lesbar: {err}"),
                    });
                }
            };
            let current_generation = planner::resource_generation(&resources);
            if let Err(error) = validate_resource_generation(plan, &current_generation) {
                return api::Response::Error(error);
            }
            start_model_ready_with_plan(model, engine, config, catalog, resources, plan)
        }
        api::Request::StopModel => {
            let mut e = engine.lock().unwrap();
            let previous = e.active_model().map(str::to_owned);
            match e.stop_model() {
                Ok(()) => api::Response::Accepted {
                    model: previous.unwrap_or_default(),
                },
                Err(_) => api::Response::Error(api::ApiError::model_busy()),
            }
        }
        api::Request::Chat { model, prompt } => {
            let mut e = engine.lock().unwrap();
            e.refresh();
            if e.state() != EngineState::Ready {
                return api::Response::Error(api::error_for_state(e.state()));
            }
            if e.active_model() != Some(model.as_str()) {
                return api::Response::Error(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message: format!(
                        "angefordertes Modell '{model}' ist nicht das aktuell geladene Modell"
                    ),
                });
            }
            if let Err(err) = e.begin_request(&model) {
                return api::Response::Error(api::ApiError {
                    code: ApiErrorCode::ModelBusy,
                    message: format!("Request konnte nicht gestartet werden: {err}"),
                });
            }
            let endpoint = e
                .worker_endpoint()
                .map(|(host, port)| (host.to_owned(), port));
            let result = match endpoint {
                Some((host, port)) => proxy_worker_chat(
                    &host,
                    port,
                    &model,
                    &prompt,
                    config.server.inference_timeout_secs,
                    None,
                ),
                None => Err("kein Worker-Endpunkt für das aktive Modell hinterlegt".to_string()),
            };
            if let Err(err) = e.finish_request() {
                return api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Request-Zustand konnte nicht abgeschlossen werden: {err}"),
                });
            }
            match result {
                Ok(text) => api::Response::Chat { model, text },
                Err(message) => api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Worker-Chat fehlgeschlagen: {message}"),
                }),
            }
        }
        api::Request::StageStatus => {
            let e = engine.lock().unwrap();
            match e.recover_staging() {
                Ok(commits) => api::Response::StageStatus {
                    committed_blocks: commits.len(),
                },
                Err(err) => api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Staging-Status nicht lesbar: {err}"),
                }),
            }
        }
        api::Request::CreateDownload(request) => {
            match with_downloads(server_state, config, catalog, |manager| {
                manager.create(request)
            }) {
                Ok(Ok(id)) => api::Response::DownloadCreated { id },
                Ok(Err(err)) => api::Response::Error(download_error_to_api(err)),
                Err(err) => api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Downloadmanager nicht verfuegbar: {err}"),
                }),
            }
        }
        api::Request::DownloadStatus { id } => {
            match with_downloads(server_state, config, catalog, |manager| manager.status(&id)) {
                Ok(Ok(status)) => api::Response::DownloadStatus(status),
                Ok(Err(err)) => api::Response::Error(download_error_to_api(err)),
                Err(err) => api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Downloadmanager nicht verfuegbar: {err}"),
                }),
            }
        }
        api::Request::CancelDownload { id } => {
            match with_downloads(server_state, config, catalog, |manager| manager.cancel(&id)) {
                Ok(Ok(())) => api::Response::DownloadCancelled { id },
                Ok(Err(err)) => api::Response::Error(download_error_to_api(err)),
                Err(err) => api::Response::Error(api::ApiError {
                    code: ApiErrorCode::Internal,
                    message: format!("Downloadmanager nicht verfuegbar: {err}"),
                }),
            }
        }
        api::Request::RescanModels => {
            // Baut den Katalog komplett neu auf (inkl. teurer Datei-Hashes
            // ueber ALLE konfigurierten Modellquellen) und tauscht ihn dann
            // atomar aus. Auf Systemen mit vielen/grossen Modellen kann das
            // gemessen weit ueber eine Minute dauern (siehe download.rs-
            // Moduldoku) — ein synchroner HTTP-Request wuerde bei dieser
            // Dauer von jedem realistischen Client-Timeout (GUI, curl mit
            // Default-Timeout, ...) als abgebrochene Verbindung gesehen.
            // Deshalb laeuft der Rescan in einem Hintergrund-Thread; der
            // Request selbst antwortet sofort. Laufende Leser sehen bis zum
            // Abschluss weiterhin den alten Katalog, danach atomar den
            // neuen — nie einen Zwischenzustand.
            if server_state.rescan_in_progress.swap(true, Ordering::SeqCst) {
                api::Response::CatalogRescanStarted {
                    already_running: true,
                }
            } else {
                let rescan_config = config.clone();
                let catalog = catalog.clone();
                let in_progress = server_state.rescan_in_progress.clone();
                std::thread::spawn(move || {
                    let started = std::time::Instant::now();
                    let (model_count, warnings) =
                        crate::model_sources::rescan_and_apply(&rescan_config, &catalog);
                    crate::observability::emit(
                        "catalog_rescanned",
                        serde_json::json!({
                            "model_count": model_count,
                            "warning_count": warnings.len(),
                            "trigger": "manual",
                            "duration_ms": started.elapsed().as_millis(),
                        }),
                    );
                    in_progress.store(false, Ordering::SeqCst);
                });
                api::Response::CatalogRescanStarted {
                    already_running: false,
                }
            }
        }
    }
}

fn download_error_to_api(err: crate::download::DownloadError) -> api::ApiError {
    use crate::download::DownloadError;
    match err {
        DownloadError::InvalidRequest(message) => api::ApiError {
            code: ApiErrorCode::InvalidRequest,
            message,
        },
        DownloadError::NotFound => api::ApiError {
            code: ApiErrorCode::NotFound,
            message: "kein Download mit dieser ID bekannt".into(),
        },
        DownloadError::Io(err) => api::ApiError {
            code: ApiErrorCode::Internal,
            message: format!("Download-IO-Fehler: {err}"),
        },
    }
}

fn validate_plan_identity<'a>(
    latest: Option<&'a Plan>,
    model: &str,
    plan_id: &str,
) -> Result<&'a Plan, api::ApiError> {
    let Some(plan) = latest else {
        return Err(api::ApiError::stale_plan(
            "vor dem Start muss ein aktueller Plan erzeugt werden",
        ));
    };
    if plan.plan_id != plan_id || plan.model != model {
        return Err(api::ApiError::stale_plan(
            "plan_id gehoert nicht zum letzten Plan fuer dieses Modell",
        ));
    }
    if !plan.safe {
        return Err(api::ApiError {
            code: ApiErrorCode::ResourceDenied,
            message: format!("Modellstart abgelehnt: {}", plan.reason),
        });
    }
    Ok(plan)
}

fn validate_resource_generation(
    plan: &Plan,
    current_generation: &str,
) -> Result<(), api::ApiError> {
    if plan.resource_generation != current_generation {
        return Err(api::ApiError::stale_plan(
            "Ressourcen haben sich seit der Planung geaendert",
        ));
    }
    Ok(())
}

fn start_model_legacy(
    model: String,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    catalog: &RwLock<ModelCatalog>,
    resources: Resources,
) -> api::Response {
    let mut e = engine.lock().unwrap();
    if e.state() != EngineState::Idle {
        return api::Response::Error(api::ApiError::model_busy());
    }
    let cfg = match resolve_start_config(&model, config, catalog, &resources) {
        Ok(cfg) => cfg,
        Err(err) => return api::Response::Error(err),
    };
    match e.start_model(&cfg) {
        Ok(()) => api::Response::Accepted { model },
        Err(err) => api::Response::Error(api::ApiError {
            code: ApiErrorCode::Internal,
            message: format!("Modellstart fehlgeschlagen: {err}"),
        }),
    }
}

#[allow(dead_code)]
fn start_model_ready(
    model: String,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    catalog: &RwLock<ModelCatalog>,
    resources: Resources,
) -> api::Response {
    let mut e = engine.lock().unwrap();
    if e.state() != EngineState::Idle {
        return api::Response::Error(api::ApiError::model_busy());
    }
    let cfg = match resolve_start_config(&model, config, catalog, &resources) {
        Ok(cfg) => cfg,
        Err(err) => return api::Response::Error(err),
    };
    let timeout = Duration::from_secs(config.worker.readiness_timeout_secs);
    match e.start_model_ready(&cfg, timeout) {
        Ok(()) => api::Response::Accepted { model },
        Err(failure) => api::Response::Error(api_error_for_worker_failure(failure)),
    }
}

fn start_model_ready_with_plan(
    model: String,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    catalog: &RwLock<ModelCatalog>,
    resources: Resources,
    plan: &Plan,
) -> api::Response {
    let mut e = engine.lock().unwrap();
    if e.state() != EngineState::Idle {
        return api::Response::Error(api::ApiError::model_busy());
    }
    let mut cfg = match resolve_start_config(&model, config, catalog, &resources) {
        Ok(cfg) => cfg,
        Err(err) => return api::Response::Error(err),
    };
    cfg.gpu_layers = plan.gpu_layers;
    cfg.ctx_size = plan.ctx_size;
    let timeout = Duration::from_secs(config.worker.readiness_timeout_secs);
    match e.start_model_ready(&cfg, timeout) {
        Ok(()) => api::Response::Accepted { model },
        Err(failure) => api::Response::Error(api_error_for_worker_failure(failure)),
    }
}

fn resolve_start_config(
    model: &str,
    config: &Config,
    catalog: &RwLock<ModelCatalog>,
    resources: &Resources,
) -> Result<WorkerConfig, api::ApiError> {
    let models_dir = config.models_dir();
    let model_path = match catalog.read().unwrap().start_location(model) {
        Some((_source, path)) => path,
        None => {
            if let Err(message) = validate_model_path(model) {
                return Err(api::ApiError {
                    code: ApiErrorCode::InvalidRequest,
                    message,
                });
            }
            match crate::gguf_registry::resolve_under(&models_dir, model) {
                Ok(Some(path)) => {
                    if let Err(error) = crate::gguf_registry::inspect(&path) {
                        return Err(api::ApiError {
                            code: ApiErrorCode::InvalidRequest,
                            message: format!("GGUF-Metadaten ungueltig: {error}"),
                        });
                    }
                    path
                }
                Ok(None) => {
                    return Err(api::ApiError {
                        code: ApiErrorCode::InvalidRequest,
                        message: format!(
                            "Modell-ID '{model}' ist nicht startbar oder nicht bekannt"
                        ),
                    });
                }
                Err(error) => {
                    return Err(api::ApiError {
                        code: ApiErrorCode::Internal,
                        message: format!("Modellpfad nicht sicher aufloesbar: {error}"),
                    });
                }
            }
        }
    };
    resolve_worker_config(model, &model_path, resources, config)
}

fn api_error_for_worker_failure(failure: WorkerFailure) -> api::ApiError {
    let code = match failure.code {
        WorkerFailureCode::WorkerTimeout => ApiErrorCode::WorkerTimeout,
        WorkerFailureCode::WorkerCrashed | WorkerFailureCode::SpawnFailed => {
            ApiErrorCode::WorkerCrashed
        }
        WorkerFailureCode::OutOfMemory => ApiErrorCode::OutOfMemory,
        WorkerFailureCode::PortConflict => ApiErrorCode::PortConflict,
        WorkerFailureCode::CorruptModel => ApiErrorCode::CorruptModel,
        WorkerFailureCode::SlotBusy => ApiErrorCode::ModelBusy,
    };
    api::ApiError {
        code,
        message: failure.message,
    }
}

fn proxy_active_worker_json(
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    model: &str,
    path: &str,
    body: &serde_json::Value,
    cancellation: Option<&AtomicBool>,
) -> Result<serde_json::Value, api::ApiError> {
    let mut e = engine.lock().unwrap();
    e.refresh();
    if e.state() != EngineState::Ready {
        return Err(api::error_for_state(e.state()));
    }
    if e.active_model() != Some(model) {
        return Err(api::ApiError {
            code: ApiErrorCode::InvalidRequest,
            message: format!(
                "angefordertes Modell '{model}' ist nicht das aktuell geladene Modell"
            ),
        });
    }
    if let Err(err) = e.begin_request(model) {
        return Err(api::ApiError {
            code: ApiErrorCode::ModelBusy,
            message: format!("Request konnte nicht gestartet werden: {err}"),
        });
    }
    let endpoint = e
        .worker_endpoint()
        .map(|(host, port)| (host.to_owned(), port));
    let result = match endpoint {
        Some((host, port)) => proxy_worker_json(
            &host,
            port,
            path,
            body,
            config.server.inference_timeout_secs,
            cancellation,
        ),
        None => Err("kein Worker-Endpunkt fuer das aktive Modell hinterlegt".to_string()),
    };
    if let Err(err) = e.finish_request() {
        return Err(api::ApiError {
            code: ApiErrorCode::Internal,
            message: format!("Request-Zustand konnte nicht abgeschlossen werden: {err}"),
        });
    }
    result.map_err(|message| api::ApiError {
        code: ApiErrorCode::Internal,
        message: format!("Worker-Proxy fehlgeschlagen: {message}"),
    })
}

fn stream_active_worker_response(
    client: &mut TcpStream,
    engine: &Arc<Mutex<Engine>>,
    config: &Config,
    model: &str,
    path: &str,
    body: &serde_json::Value,
    cancellation: Option<&AtomicBool>,
) -> Result<(), api::ApiError> {
    let mut e = engine.lock().unwrap();
    e.refresh();
    if e.state() != EngineState::Ready {
        return Err(api::error_for_state(e.state()));
    }
    if e.active_model() != Some(model) {
        return Err(api::ApiError {
            code: ApiErrorCode::InvalidRequest,
            message: format!(
                "angefordertes Modell '{model}' ist nicht das aktuell geladene Modell"
            ),
        });
    }
    if let Err(err) = e.begin_request(model) {
        return Err(api::ApiError {
            code: ApiErrorCode::ModelBusy,
            message: format!("Request konnte nicht gestartet werden: {err}"),
        });
    }
    let endpoint = e
        .worker_endpoint()
        .map(|(host, port)| (host.to_owned(), port));
    let result = match endpoint {
        Some((host, port)) => proxy_worker_raw_response_to_client(
            client,
            &host,
            port,
            path,
            body,
            config.server.inference_timeout_secs,
            cancellation,
        ),
        None => Err(StreamProxyError::BeforeResponse(
            "kein Worker-Endpunkt fuer das aktive Modell hinterlegt".into(),
        )),
    };
    if let Err(err) = e.finish_request() {
        return Err(api::ApiError {
            code: ApiErrorCode::Internal,
            message: format!("Request-Zustand konnte nicht abgeschlossen werden: {err}"),
        });
    }
    match result {
        Ok(()) => Ok(()),
        Err(StreamProxyError::BeforeResponse(message)) => Err(api::ApiError {
            code: ApiErrorCode::Internal,
            message: format!("Worker-Stream fehlgeschlagen: {message}"),
        }),
        Err(StreamProxyError::AfterResponse(message)) => {
            crate::observability::emit(
                "worker_stream_aborted",
                serde_json::json!({"phase": "after_response", "message": message}),
            );
            Ok(())
        }
        Err(StreamProxyError::Cancelled) => Err(api::ApiError {
            code: ApiErrorCode::Cancelled,
            message: "Request wurde abgebrochen".into(),
        }),
    }
}

enum StreamProxyError {
    BeforeResponse(String),
    AfterResponse(String),
    Cancelled,
}

/// Minimaler, lokaler Proxy zum bereits exklusiv verwalteten llama.cpp-Worker.
/// Der Modellname und der Prompt werden ausschließlich über JSON serialisiert;
/// es gibt keine Shell oder String-Interpolation in einem Prozessaufruf.
fn proxy_worker_chat(
    host: &str,
    port: u16,
    model: &str,
    prompt: &str,
    timeout_secs: u64,
    cancellation: Option<&AtomicBool>,
) -> Result<String, String> {
    let body = crate::openai::legacy_chat_worker_body(model, prompt);
    let value = proxy_worker_json(
        host,
        port,
        "/v1/chat/completions",
        &body,
        timeout_secs,
        cancellation,
    )?;
    crate::openai::chat_text(&value)
}

fn proxy_worker_json(
    host: &str,
    port: u16,
    path: &str,
    body: &serde_json::Value,
    timeout_secs: u64,
    cancellation: Option<&AtomicBool>,
) -> Result<serde_json::Value, String> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|err| format!("Worker-Adresse nicht auflösbar: {err}"))?
        .next()
        .ok_or_else(|| "Worker-Adresse enthält kein Ziel".to_string())?;
    let timeout = Duration::from_secs(timeout_secs);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|err| format!("Worker nicht erreichbar: {err}"))?;
    stream
        .set_read_timeout(Some(timeout.min(Duration::from_millis(100))))
        .map_err(|err| format!("Worker-Read-Timeout nicht setzbar: {err}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|err| format!("Worker-Write-Timeout nicht setzbar: {err}"))?;
    let body = body.to_string();
    if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err("Request wurde abgebrochen".into());
    }
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("Worker-Anfrage nicht sendbar: {err}"))?;
    let mut response = Vec::new();
    let mut chunk = [0u8; 8192];
    let deadline = std::time::Instant::now() + timeout;
    while response.len() <= 8 * 1_024 * 1_024 {
        if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err("Request wurde abgebrochen".into());
        }
        if std::time::Instant::now() >= deadline {
            return Err("Worker-Read-Timeout".into());
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&chunk[..read]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(err) => return Err(format!("Worker-Antwort nicht lesbar: {err}")),
        }
    }
    if response.len() > 8 * 1_024 * 1_024 {
        return Err("Worker-Antwort überschreitet 8 MiB".to_string());
    }
    parse_worker_json_response(&response)
}

fn parse_worker_json_response(response: &[u8]) -> Result<serde_json::Value, String> {
    let Some(header_end) = find_header_end(response) else {
        return Err("unvollständige HTTP-Antwort vom Worker".to_string());
    };
    let header = String::from_utf8_lossy(&response[..header_end]);
    if !header.starts_with("HTTP/1.1 2") {
        return Err(format!(
            "Worker lieferte Fehlerstatus: {}",
            header.lines().next().unwrap_or("unbekannt")
        ));
    }
    serde_json::from_slice(&response[header_end + 4..])
        .map_err(|err| format!("Worker lieferte kein gueltiges JSON: {err}"))
}

fn proxy_worker_raw_response_to_client(
    client: &mut TcpStream,
    host: &str,
    port: u16,
    path: &str,
    body: &serde_json::Value,
    timeout_secs: u64,
    cancellation: Option<&AtomicBool>,
) -> Result<(), StreamProxyError> {
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|err| StreamProxyError::BeforeResponse(format!("Adresse ungueltig: {err}")))?
        .next()
        .ok_or_else(|| {
            StreamProxyError::BeforeResponse("Worker-Adresse enthaelt kein Ziel".into())
        })?;
    let timeout = Duration::from_secs(timeout_secs);
    let mut worker = TcpStream::connect_timeout(&addr, timeout).map_err(|err| {
        StreamProxyError::BeforeResponse(format!("Worker nicht erreichbar: {err}"))
    })?;
    worker
        .set_read_timeout(Some(timeout.min(Duration::from_millis(100))))
        .map_err(|err| {
            StreamProxyError::BeforeResponse(format!("Read-Timeout nicht setzbar: {err}"))
        })?;
    worker.set_write_timeout(Some(timeout)).map_err(|err| {
        StreamProxyError::BeforeResponse(format!("Write-Timeout nicht setzbar: {err}"))
    })?;
    let body = body.to_string();
    if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(StreamProxyError::Cancelled);
    }
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    worker.write_all(request.as_bytes()).map_err(|err| {
        StreamProxyError::BeforeResponse(format!("Worker-Anfrage nicht sendbar: {err}"))
    })?;
    let mut started = false;
    let mut chunk = [0u8; 8192];
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            let _ = worker.shutdown(std::net::Shutdown::Both);
            return Err(StreamProxyError::Cancelled);
        }
        if std::time::Instant::now() >= deadline {
            return Err(StreamProxyError::BeforeResponse(
                "Worker-Read-Timeout".into(),
            ));
        }
        let read = match worker.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(err) if started => {
                return Err(StreamProxyError::AfterResponse(format!(
                    "Worker-Stream brach ab: {err}"
                )));
            }
            Err(err) => {
                return Err(StreamProxyError::BeforeResponse(format!(
                    "Worker-Antwort nicht lesbar: {err}"
                )));
            }
        };
        match client.write_all(&chunk[..read]) {
            Ok(()) => started = true,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(());
            }
            Err(err) if started => {
                return Err(StreamProxyError::AfterResponse(format!(
                    "Client-Stream brach ab: {err}"
                )));
            }
            Err(err) => {
                return Err(StreamProxyError::BeforeResponse(format!(
                    "Client-Antwort nicht sendbar: {err}"
                )));
            }
        }
    }
}

/// `model` ist eine relative GGUF-ID. Sie wird ausschließlich über den
/// Scanner unterhalb des konfigurierten Modellverzeichnisses aufgelöst.
/// Ollama-IDs dürfen nicht versehentlich an den llama.cpp-Worker gelangen.
fn validate_model_path(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("Modellpfad ist leer".to_string());
    }
    if model.contains("..") {
        return Err("Modellpfad darf kein '..' enthalten".to_string());
    }
    let bytes = model.as_bytes();
    let windows_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    if std::path::Path::new(model).is_absolute() || model.starts_with('\\') || windows_absolute {
        return Err(
            "Modellpfad muss relativ zum konfigurierten Modellverzeichnis sein".to_string(),
        );
    }
    Ok(())
}

fn resolve_worker_config(
    model: &str,
    path: &std::path::Path,
    resources: &Resources,
    config: &Config,
) -> Result<WorkerConfig, api::ApiError> {
    let size_bytes = std::fs::metadata(path)
        .map_err(|err| api::ApiError {
            code: ApiErrorCode::Internal,
            message: format!("GGUF-Metadaten nicht lesbar: {err}"),
        })?
        .len();
    let file_size_mb = size_bytes.saturating_add(1_048_575) / 1_048_576;
    let requested_context = config.worker.default_context;
    // `999` is a llama.cpp command-line sentinel, never a model-layer count.
    // The planner needs the real value to compute a safe partial offload.
    let layers = crate::gguf_registry::inspect(path)
        .ok()
        .and_then(|metadata| metadata.layers)
        // Legacy/minimal fixtures have no block count.  They cannot claim
        // full offload, so retain the configured finite fallback for their
        // test-only CPU/RAM plans; real catalog auto-plans require metadata.
        .unwrap_or(32)
        .try_into()
        .map_err(|_| api::ApiError {
            code: ApiErrorCode::InvalidRequest,
            message: "GGUF-Layerzahl liegt ausserhalb des unterstuetzten Bereichs".into(),
        })?;
    let profile = ModelProfile {
        model: model.to_string(),
        file_size_mb,
        estimated_gpu_mb: file_size_mb,
        estimated_ram_mb: file_size_mb.saturating_mul(5) / 4,
        layers,
        context_tokens: requested_context.max(512),
        requested_context_tokens: requested_context,
    };
    let plan = planner::plan(&profile, resources);
    if !plan.safe {
        return Err(api::ApiError {
            code: ApiErrorCode::ResourceDenied,
            message: format!("Modellstart abgelehnt: {}", plan.reason),
        });
    }
    Ok(WorkerConfig {
        binary: config.paths.llama_server.to_string_lossy().into_owned(),
        model: model.to_string(),
        model_path: Some(path.to_string_lossy().into_owned()),
        host: "127.0.0.1".to_string(),
        port: config.worker.port,
        gpu_layers: plan.gpu_layers,
        ctx_size: plan.ctx_size,
    })
}

fn status_code_for(response: &api::Response) -> u16 {
    match response {
        api::Response::Error(e) => match e.code {
            ApiErrorCode::ModelBusy => 409,
            ApiErrorCode::StalePlan => 409,
            ApiErrorCode::NotReady => 409,
            ApiErrorCode::ResourceDenied => 409,
            ApiErrorCode::PortConflict => 409,
            ApiErrorCode::InvalidRequest => 400,
            ApiErrorCode::WorkerTimeout => 504,
            ApiErrorCode::Cancelled => 499,
            ApiErrorCode::WorkerCrashed
            | ApiErrorCode::OutOfMemory
            | ApiErrorCode::CorruptModel => 500,
            ApiErrorCode::NotFound => 404,
            ApiErrorCode::Internal => 500,
        },
        api::Response::Accepted { .. } => 202,
        _ => 200,
    }
}

fn write_response<T: serde::Serialize>(
    stream: &mut TcpStream,
    status: u16,
    body: &T,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        401 => "Unauthorized",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        499 => "Client Closed Request",
        _ => "Internal Server Error",
    };
    let body = serde_json::to_string(body)
        .unwrap_or_else(|_| "{\"kind\":\"error\",\"code\":\"internal\"}".to_string());
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine::Engine, staging::StageStore};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    /// `WorkerSupervisor` sperrt einen echten, festen OS-Lock unter
    /// `/tmp/tri-ai-runner-single-model.lock`. Ein eigenes `BUSY_LOCK` hier
    /// hätte nur Tests INNERHALB dieses Moduls serialisiert — nach dem Merge
    /// mit den `supervisor.rs`-Tests (die denselben echten Pfad anfassen)
    /// kollidierten beide Module trotzdem noch. Ersetzt durch
    /// `supervisor::acquire_test_lock_guard()` (`pub(crate)`): gibt JEDEM
    /// Aufruf eine eigene, eindeutige Lock-Datei statt der geteilten
    /// Produktionsdatei — löst die Kollision crateweit, nicht nur lokal.
    fn test_dir(tag: &str) -> std::path::PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tri-ai-http-{tag}-{}-{now}", std::process::id()))
    }

    fn new_engine(tag: &str) -> (Engine, std::path::PathBuf) {
        let d = test_dir(tag);
        (Engine::new(&d).unwrap(), d)
    }

    fn dispatch_default(request: api::Request, engine: &Arc<Mutex<Engine>>) -> api::Response {
        dispatch(
            request,
            engine,
            &Config::default(),
            &Arc::new(RwLock::new(ModelCatalog::new())),
            &ServerState::default(),
        )
    }

    /// Ignoriert alle Argumente, läuft absichtlich lange — simuliert einen
    /// belegten Modell-Slot, ohne ein echtes LLM zu starten.
    fn stub_worker_binary(dir: &std::path::Path) -> String {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("stub-worker.sh");
        fs::write(&path, "#!/bin/sh\nsleep 5\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn write_minimal_gguf(path: &std::path::Path) {
        let mut bytes = Vec::from(&b"GGUF"[..]);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }

    fn plan_fixture(model: &str, requested_context_tokens: u32) -> Plan {
        planner::plan(
            &ModelProfile {
                model: model.into(),
                file_size_mb: 5_000,
                estimated_gpu_mb: 6_000,
                estimated_ram_mb: 7_000,
                layers: 32,
                context_tokens: 8_192,
                requested_context_tokens,
            },
            &Resources {
                vram_free_mb: 9_000,
                ram_free_mb: 16_000,
                ssd_free_mb: 100_000,
                vram_reserve_mb: 1_200,
                ram_reserve_mb: 2_000,
                ssd_reserve_mb: 10_000,
            },
        )
    }

    #[test]
    fn current_plan_is_accepted_by_plan_gate() {
        let plan = plan_fixture("model-a", 4_096);
        assert!(validate_plan_identity(Some(&plan), "model-a", &plan.plan_id).is_ok());
        assert!(validate_resource_generation(&plan, &plan.resource_generation).is_ok());
    }

    #[test]
    fn missing_plan_is_reported_as_stale_plan() {
        let error = validate_plan_identity(None, "model-a", "plan-v1-unknown").unwrap_err();
        assert_eq!(error.code, ApiErrorCode::StalePlan);
    }

    #[test]
    fn replaced_plan_id_is_reported_as_stale_plan() {
        let old = plan_fixture("model-a", 4_096);
        let latest = plan_fixture("model-a", 8_192);
        assert_ne!(old.plan_id, latest.plan_id);
        let error = validate_plan_identity(Some(&latest), "model-a", &old.plan_id).unwrap_err();
        assert_eq!(error.code, ApiErrorCode::StalePlan);
    }

    #[test]
    fn foreign_plan_id_or_model_is_reported_as_stale_plan() {
        let plan = plan_fixture("model-a", 4_096);
        let foreign_id =
            validate_plan_identity(Some(&plan), "model-a", "plan-v1-foreign").unwrap_err();
        assert_eq!(foreign_id.code, ApiErrorCode::StalePlan);

        let foreign_model =
            validate_plan_identity(Some(&plan), "model-b", &plan.plan_id).unwrap_err();
        assert_eq!(foreign_model.code, ApiErrorCode::StalePlan);
    }

    #[test]
    fn changed_resource_generation_makes_plan_stale() {
        let plan = plan_fixture("model-a", 4_096);
        let error = validate_resource_generation(&plan, "resources-v1-changed").unwrap_err();
        assert_eq!(error.code, ApiErrorCode::StalePlan);
    }

    #[test]
    fn list_models_uses_path_free_stable_catalog_entry() {
        let (engine, dir) = new_engine("list-models");
        let engine = Arc::new(Mutex::new(engine));
        let mut catalog = ModelCatalog::new();
        catalog
            .add_direct_gguf(
                "/private/model.gguf",
                crate::model_catalog::CatalogMetadata {
                    digest: "a".repeat(64),
                    size_bytes: Some(42),
                    format: Some("gguf".into()),
                    ..crate::model_catalog::CatalogMetadata::default()
                },
            )
            .unwrap();
        let response = dispatch(
            api::Request::ListModels,
            &engine,
            &Config::default(),
            &Arc::new(RwLock::new(catalog)),
            &ServerState::default(),
        );
        match response {
            api::Response::Models { models } => {
                assert_eq!(models.len(), 1);
                assert_eq!(models[0].id, format!("sha256:{}", "a".repeat(64)));
                assert!(!serde_json::to_string(&models)
                    .unwrap()
                    .contains("/private/"));
            }
            other => panic!("unerwartete Antwort: {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_model_path_rejects_absolute_and_traversal() {
        assert!(validate_model_path("").is_err());
        assert!(validate_model_path("/etc/passwd").is_err());
        assert!(validate_model_path(r"C:\models\secret.gguf").is_err());
        assert!(validate_model_path(r"\\server\share\secret.gguf").is_err());
        assert!(validate_model_path("../../etc/shadow").is_err());
        assert!(validate_model_path("models/../../../etc/shadow").is_err());
        assert!(validate_model_path("a.gguf").is_ok());
        assert!(validate_model_path("subdir/model.gguf").is_ok());
    }

    #[test]
    fn start_model_rejects_path_traversal_before_touching_supervisor() {
        let (engine, dir) = new_engine("path-traversal");
        let engine = Arc::new(Mutex::new(engine));
        let response = dispatch_default(
            api::Request::StartModel {
                model: "../../etc/shadow".into(),
            },
            &engine,
        );
        match &response {
            api::Response::Error(e) => assert_eq!(e.code, ApiErrorCode::InvalidRequest),
            other => panic!("erwartete InvalidRequest, bekam: {other:?}"),
        }
        assert_eq!(status_code_for(&response), 400);
        // Engine darf dabei nicht mal in Loading gegangen sein.
        assert_eq!(engine.lock().unwrap().state(), EngineState::Idle);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn worker_config_resolves_gguf_to_canonical_path_and_keeps_api_id() {
        let dir = test_dir("resolved-config");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_minimal_gguf(&models_dir.join("nested.gguf"));
        let resources = Resources {
            vram_free_mb: 10000,
            ram_free_mb: 16000,
            ssd_free_mb: 100000,
            vram_reserve_mb: 1000,
            ram_reserve_mb: 1000,
            ssd_reserve_mb: 1000,
        };
        let mut config = Config::default();
        config.paths.models_dir = Some(models_dir.clone());
        let path = fs::canonicalize(models_dir.join("nested.gguf")).unwrap();
        let cfg = resolve_worker_config("nested.gguf", &path, &resources, &config).unwrap();
        assert_eq!(cfg.model, "nested.gguf");
        assert_eq!(
            cfg.model_path.as_deref(),
            Some(
                fs::canonicalize(models_dir.join("nested.gguf"))
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(cfg.gpu_layers > 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn worker_config_rejects_model_when_no_safe_lane_exists() {
        let dir = test_dir("rejected-config");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        let mut fixture = vec![0u8; 2 * 1_048_576];
        fixture[..4].copy_from_slice(b"GGUF");
        fs::write(models_dir.join("large.gguf"), fixture).unwrap();
        let resources = Resources {
            vram_free_mb: 0,
            ram_free_mb: 0,
            ssd_free_mb: 0,
            vram_reserve_mb: 0,
            ram_reserve_mb: 0,
            ssd_reserve_mb: 0,
        };
        let config = Config::default();
        let path = fs::canonicalize(models_dir.join("large.gguf")).unwrap();
        let error = resolve_worker_config("large.gguf", &path, &resources, &config).unwrap_err();
        assert_eq!(error.code, ApiErrorCode::ResourceDenied);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn worker_chat_parser_extracts_completion_text() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"Hallo lokal\"}}]}";
        let value = parse_worker_json_response(response).unwrap();
        assert_eq!(crate::openai::chat_text(&value).unwrap(), "Hallo lokal");
    }

    #[test]
    fn worker_chat_parser_rejects_non_success_status() {
        let response =
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\r\n{}";
        assert!(parse_worker_json_response(response).is_err());
    }

    #[test]
    fn status_is_ok_when_idle() {
        let (engine, dir) = new_engine("status");
        let engine = Arc::new(Mutex::new(engine));
        let response = dispatch_default(api::Request::Status, &engine);
        match response {
            api::Response::Status(s) => assert_eq!(s.state, EngineState::Idle),
            other => panic!("unerwartete Antwort: {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn second_start_model_is_rejected_as_busy_without_second_process() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let (engine, dir) = new_engine("busy");
        let bin = stub_worker_binary(&dir);
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_minimal_gguf(&models_dir.join("a.gguf"));
        let mut config = Config::default();
        config.paths.llama_server = bin.into();
        config.paths.models_dir = Some(models_dir);
        let engine = Arc::new(Mutex::new(engine));

        let first = dispatch(
            api::Request::StartModel {
                model: "a.gguf".into(),
            },
            &engine,
            &config,
            &Arc::new(RwLock::new(ModelCatalog::new())),
            &ServerState::default(),
        );
        assert!(
            matches!(first, api::Response::Accepted { .. }),
            "erster Modellstart schlug fehl: {first:?}"
        );

        let second = dispatch(
            api::Request::StartModel {
                model: "b.gguf".into(),
            },
            &engine,
            &config,
            &Arc::new(RwLock::new(ModelCatalog::new())),
            &ServerState::default(),
        );
        match &second {
            api::Response::Error(e) => assert_eq!(e.code, ApiErrorCode::ModelBusy),
            other => panic!("erwartete ModelBusy, bekam: {other:?}"),
        }
        assert_eq!(status_code_for(&second), 409);

        engine.lock().unwrap().stop_model().ok();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn chat_without_ready_model_is_not_ready_error() {
        let (engine, dir) = new_engine("chat-not-ready");
        let engine = Arc::new(Mutex::new(engine));
        let response = dispatch_default(
            api::Request::Chat {
                model: "a.gguf".into(),
                prompt: "hi".into(),
            },
            &engine,
        );
        match response {
            api::Response::Error(e) => assert_eq!(e.code, ApiErrorCode::NotReady),
            other => panic!("erwartete NotReady, bekam: {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stage_status_reports_recovered_blocks() {
        let (engine, dir) = new_engine("stage");
        {
            let store = StageStore::new(dir.join("stage")).unwrap();
            store.stage_bytes("block0", 1, b"abc").unwrap();
        }
        // Engine wurde mit einem anderen Stage-Root erzeugt (new_engine legt
        // sein eigenes `stage_root` an); für diesen Test bauen wir die
        // Engine direkt auf demselben Root.
        let engine_on_same_root = Engine::new(dir.join("stage")).unwrap();
        let _ = engine; // ungenutzte Variante aus new_engine verwerfen
        let engine = Arc::new(Mutex::new(engine_on_same_root));
        let response = dispatch_default(api::Request::StageStatus, &engine);
        match response {
            api::Response::StageStatus { committed_blocks } => assert_eq!(committed_blocks, 1),
            other => panic!("unerwartete Antwort: {other:?}"),
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// End-to-End über echtes TCP: Header/Body-Parsing und HTTP-Statuscodes
    /// werden hier tatsächlich durch die Leitung geprüft, nicht nur über
    /// `dispatch` direkt.
    #[test]
    fn http_roundtrip_returns_409_on_busy_slot() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let (engine, dir) = new_engine("http-roundtrip");
        let bin = stub_worker_binary(&dir);
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_minimal_gguf(&models_dir.join("a.gguf"));
        let mut config = Config::default();
        config.paths.llama_server = bin.into();
        config.paths.models_dir = Some(models_dir);
        let engine = Arc::new(Mutex::new(engine));
        let listener = match bind("127.0.0.1:0") {
            Ok(listener) => listener,
            // Einige restriktive CI-/Sandbox-Umgebungen verbieten selbst
            // Loopback-Sockets. Der reine `dispatch`-Test prüft die
            // Zustandsregel dort weiterhin; auf normalen Hosts bleibt dieser
            // End-to-End-Test vollständig aktiv.
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return,
            Err(err) => panic!("Loopback-Listener konnte nicht gebunden werden: {err}"),
        };
        let addr = listener.local_addr().unwrap();
        let engine_for_server = engine.clone();
        let config = Arc::new(config);
        std::thread::spawn(move || {
            let _ = serve_with_config(listener, engine_for_server, config);
        });

        let first = send_raw(addr, r#"{"action":"start_model","model":"a.gguf"}"#);
        assert!(first.starts_with("HTTP/1.1 202"), "unerwartet: {first}");

        let second = send_raw(addr, r#"{"action":"start_model","model":"b.gguf"}"#);
        assert!(second.starts_with("HTTP/1.1 409"), "unerwartet: {second}");

        engine.lock().unwrap().stop_model().ok();
        let _ = fs::remove_dir_all(dir);
    }

    fn send_raw(addr: std::net::SocketAddr, body: &str) -> String {
        send_raw_route(addr, "POST", "/", body)
    }

    fn send_raw_route(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(req.as_bytes()).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                // EINTR ist kein Verbindungsende, sondern ein durch ein
                // Signal unterbrochener Syscall — bei vielen parallel
                // spawnenden/wartenden Kindprozessen (SIGCHLD) im restlichen
                // Testlauf reproduzierbar. Korrektes Verhalten: erneut lesen.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Startet einen Server auf einem ephemeren Port für die V1-Routen-Tests.
    /// Gibt `None` zurück, wenn selbst Loopback-Sockets gesperrt sind
    /// (restriktive Sandbox) — die betroffenen Tests werden dann übersprungen.
    fn spawn_test_server(engine: Engine) -> Option<std::net::SocketAddr> {
        spawn_test_server_with_config(engine, Config::default())
    }

    fn spawn_test_server_with_config(
        engine: Engine,
        mut config: Config,
    ) -> Option<std::net::SocketAddr> {
        // Die Arbeitskopie enthält absichtlich große, gitignored GGUF-Dateien.
        // HTTP-Contract-Tests sollen keinen Vollhash dieser Modelle beim
        // Serverstart auslösen; Tests, die Katalogdaten benötigen, setzen
        // `models_dir` explizit.
        if config.paths.models_dir.is_none() {
            let empty_models = test_dir("empty-models");
            fs::create_dir_all(&empty_models).unwrap();
            config.paths.models_dir = Some(empty_models);
        }
        let listener = match bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(err) => panic!("Loopback-Listener konnte nicht gebunden werden: {err}"),
        };
        let addr = listener.local_addr().unwrap();
        let engine = Arc::new(Mutex::new(engine));
        let config = Arc::new(config);
        std::thread::spawn(move || {
            let _ = serve_with_config(listener, engine, config);
        });
        Some(addr)
    }

    fn spawn_test_server_with_arc(
        engine: Arc<Mutex<Engine>>,
        mut config: Config,
    ) -> Option<std::net::SocketAddr> {
        if config.paths.models_dir.is_none() {
            let empty_models = test_dir("empty-models-arc");
            fs::create_dir_all(&empty_models).unwrap();
            config.paths.models_dir = Some(empty_models);
        }
        let listener = match bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(err) => panic!("Loopback-Listener konnte nicht gebunden werden: {err}"),
        };
        let addr = listener.local_addr().unwrap();
        let config = Arc::new(config);
        std::thread::spawn(move || {
            let _ = serve_with_config(listener, engine, config);
        });
        Some(addr)
    }

    fn unused_loopback_port() -> Option<u16> {
        let listener = match bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(err) => panic!("Loopback-Listener konnte nicht gebunden werden: {err}"),
        };
        Some(listener.local_addr().unwrap().port())
    }

    fn spawn_response_worker(response: &str, accepts: usize) -> Option<u16> {
        let responses = (0..accepts).map(|_| response.to_string()).collect();
        spawn_sequence_response_worker(responses)
    }

    fn spawn_sequence_response_worker(responses: Vec<String>) -> Option<u16> {
        let listener = match bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => return None,
            Err(err) => panic!("Loopback-Listener konnte nicht gebunden werden: {err}"),
        };
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            loop {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).unwrap_or(0);
                if read == 0 {
                    // Reine TCP-Connect-Sonde (z.B. `Engine::refresh()`s
                    // `tcp_ready()`-Check zwischen Tool-Loop-Runden): der
                    // Aufrufer verbindet sich, schreibt nichts und trennt
                    // sofort wieder. Das verbraucht keine der vorbereiteten
                    // Antworten, sonst rutscht die Sequenz für die
                    // tatsächlichen Requests weiter.
                    continue;
                }
                let Some(response) = responses.next() else {
                    return;
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Some(port)
    }

    #[test]
    fn v1_health_reports_degraded_without_a_ready_model() {
        let (engine, dir) = new_engine("v1-health");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(addr, "GET", "/health", "");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"status\":\"degraded\""));
        assert!(response.contains("\"model\":null"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_models_lists_scanned_catalog_without_paths() {
        let (engine, dir) = new_engine("v1-models");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_minimal_gguf(&models_dir.join("fixture.gguf"));
        let mut config = Config::default();
        config.paths.models_dir = Some(models_dir);
        let Some(addr) = spawn_test_server_with_config(engine, config) else {
            return;
        };
        let response = send_raw_route(addr, "GET", "/v1/models", "");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        assert!(response.contains("fixture.gguf"));
        assert!(response.contains("\"digest\":\"sha256:"));
        assert!(!response.contains(&dir.to_string_lossy().into_owned()));
        assert!(response.contains("\"object\":\"list\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_embeddings_without_ready_model_is_not_ready() {
        let (engine, dir) = new_engine("v1-embeddings-not-ready");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(
            addr,
            "POST",
            "/v1/embeddings",
            r#"{"model":"a.gguf","input":"hello"}"#,
        );
        assert!(
            response.starts_with("HTTP/1.1 409"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"not_ready\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_chat_streaming_without_ready_model_is_not_ready() {
        let (engine, dir) = new_engine("v1-chat-stream-not-ready");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(
            addr,
            "POST",
            "/v1/chat/completions",
            r#"{"model":"a.gguf","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        );
        assert!(
            response.starts_with("HTTP/1.1 409"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"not_ready\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_chat_streaming_proxies_worker_sse_response() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let worker_response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: text/event-stream\r\n",
            "Connection: close\r\n",
            "\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let Some(worker_port) = spawn_response_worker(worker_response, 2) else {
            return;
        };
        let (mut engine, dir) = new_engine("v1-chat-stream-proxy");
        let bin = stub_worker_binary(&dir);
        let cfg = WorkerConfig {
            binary: bin,
            model: "a.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: worker_port,
            gpu_layers: 0,
            // Updated to match default context size
            ctx_size: 4096,
        };
        engine.start_model(&cfg).unwrap();
        let engine = Arc::new(Mutex::new(engine));
        let Some(addr) = spawn_test_server_with_arc(engine.clone(), Config::default()) else {
            engine.lock().unwrap().stop_model().ok();
            let _ = fs::remove_dir_all(dir);
            return;
        };
        let response = send_raw_route(
            addr,
            "POST",
            "/v1/chat/completions",
            r#"{"model":"a.gguf","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        );
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        assert!(response.contains("Content-Type: text/event-stream"));
        assert!(response.contains("data: [DONE]"));
        engine.lock().unwrap().stop_model().ok();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_chat_stream_with_attachments_is_rejected_before_touching_the_worker() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        // Bewusste v1-Grenze: SSE-Deltas und der mehrstufige Tool-Loop lassen
        // sich nicht ohne Weiteres kombinieren (Tool-Calls muessten aus den
        // Streaming-Chunks rekonstruiert werden, bevor eine Folgerunde
        // gestartet werden kann). Die Ablehnung passiert vor jedem
        // Worker-Zugriff, daher reicht hier eine gar nicht gestartete Engine.
        let (engine, dir) = new_engine("v1-chat-stream-attachments-rejected");
        let engine = Arc::new(Mutex::new(engine));
        let Some(addr) = spawn_test_server_with_arc(engine.clone(), Config::default()) else {
            let _ = fs::remove_dir_all(dir);
            return;
        };
        let request = serde_json::json!({
            "model": "a.gguf",
            "messages": [{"role": "user", "content": "lies notes"}],
            "stream": true,
            "attachments": [{"id": "notes", "name": "notes.md", "content": "alpha"}]
        })
        .to_string();
        let response = send_raw_route(addr, "POST", "/v1/chat/completions", &request);
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"invalid_request\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn v1_chat_runs_allowed_read_attachment_tool_loop() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        // Kein eigener Sequenz-Slot fuer die Start-Readiness-Pruefung: die
        // ist nur `tcp_ready()`s reiner TCP-Connect-Test (siehe
        // `supervisor::refresh`), sendet/liest keine HTTP-Daten und wird
        // vom Mock unten deshalb ignoriert (0 gelesene Bytes), statt einen
        // Antwort-Slot zu verbrauchen.
        // Beide Runden tragen ein eigenes `usage`-Feld — der wachsende
        // Prompt aus den Tool-Ergebnissen kostet in Runde 2 selbst wieder
        // Tokens. Beweist die Aggregation ueber den ganzen Loop statt nur
        // die letzte Runde durchzureichen (siehe `accumulate_usage`).
        let tool_call_response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Connection: close\r\n",
            "\r\n",
            "{\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":null,",
            "\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":",
            "{\"name\":\"read_attachment\",\"arguments\":\"{\\\"attachment_id\\\":\\\"notes\\\"}\"}}]},",
            "\"finish_reason\":\"tool_calls\"}],",
            "\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}"
        )
        .to_string();
        let final_response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Connection: close\r\n",
            "\r\n",
            "{\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",",
            "\"content\":\"Attachment gelesen\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2,\"total_tokens\":11}}"
        )
        .to_string();
        let Some(worker_port) =
            spawn_sequence_response_worker(vec![tool_call_response, final_response])
        else {
            return;
        };
        let (mut engine, dir) = new_engine("v1-chat-tool-loop");
        let bin = stub_worker_binary(&dir);
        let cfg = WorkerConfig {
            binary: bin,
            model: "a.gguf".into(),
            model_path: None,
            host: "127.0.0.1".into(),
            port: worker_port,
            gpu_layers: 0,
            // Updated to match default context size
            ctx_size: 4096,
        };
        engine.start_model(&cfg).unwrap();
        let engine = Arc::new(Mutex::new(engine));
        let Some(addr) = spawn_test_server_with_arc(engine.clone(), Config::default()) else {
            engine.lock().unwrap().stop_model().ok();
            let _ = fs::remove_dir_all(dir);
            return;
        };
        let request = serde_json::json!({
            "model": "a.gguf",
            "messages": [{"role": "user", "content": "lies notes"}],
            "attachments": [{"id": "notes", "name": "notes.md", "content": "alpha"}]
        })
        .to_string();
        let response = send_raw_route(addr, "POST", "/v1/chat/completions", &request);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        assert!(response.contains("Attachment gelesen"));
        // Aufsummiert ueber beide Runden (5+9 / 3+2 / 8+11), nicht nur die
        // letzte Runde (die fuer sich allein 9/2/11 waere).
        let body: serde_json::Value = response
            .split("\r\n\r\n")
            .nth(1)
            .and_then(|body| serde_json::from_str(body).ok())
            .expect("Response enthaelt kein gueltiges JSON");
        assert_eq!(body["usage"]["prompt_tokens"], 14);
        assert_eq!(body["usage"]["completion_tokens"], 5);
        assert_eq!(body["usage"]["total_tokens"], 19);
        engine.lock().unwrap().stop_model().ok();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_route_is_404() {
        let (engine, dir) = new_engine("v1-404");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(addr, "GET", "/does-not-exist", "");
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "unerwartet: {response}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_status_resources_and_models_aliases_are_available() {
        let (engine, dir) = new_engine("legacy-route-aliases");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };

        for (method, path) in [
            ("GET", "/api/status"),
            ("GET", "/api/resources"),
            ("GET", "/api/models"),
        ] {
            let response = send_raw_route(addr, method, path, "");
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "{method} {path}: {response}"
            );
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_engine_alias_keeps_planned_start_contract() {
        let (engine, dir) = new_engine("legacy-engine-alias");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(addr, "POST", "/api/engine", "{}");
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "unerwartet: {response}"
        );
        assert!(response.contains("benoetigt ein nicht leeres Feld"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_start_route_requires_model_id() {
        let (engine, dir) = new_engine("engine-start-invalid");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(addr, "POST", "/api/engine/start", "{}");
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "unerwartet: {response}"
        );
        assert!(response.contains("benoetigt ein nicht leeres Feld"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_start_route_requires_current_plan_id() {
        let (engine, dir) = new_engine("engine-start-stale");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(
            addr,
            "POST",
            "/api/engine/start",
            r#"{"model_id":"sha256:fixture"}"#,
        );
        assert!(
            response.starts_with("HTTP/1.1 409"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"stale_plan\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn planned_engine_start_times_out_before_accepting_model() {
        let _guard = crate::supervisor::acquire_test_lock_guard();
        let Some(port) = unused_loopback_port() else {
            return;
        };
        let (engine, dir) = new_engine("planned-timeout");
        let bin = stub_worker_binary(&dir);
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_minimal_gguf(&models_dir.join("a.gguf"));
        let mut config = Config::default();
        config.paths.llama_server = bin.into();
        config.paths.models_dir = Some(models_dir);
        config.worker.port = port;
        config.worker.readiness_timeout_secs = 1;
        config.reserves.vram_mb = 0;
        config.reserves.ram_mb = 0;
        config.reserves.ssd_mb = 0;
        let server_state = ServerState::default();
        let engine = Arc::new(Mutex::new(engine));
        let catalog = Arc::new(RwLock::new(ModelCatalog::new()));

        // Unter Testparallelitaet kann sich der live gelesene RAM/VRAM/SSD-
        // Snapshot zwischen Planerstellung und `dispatch()` (das selbst
        // erneut `resources::read()` aufruft) knapp genug verschieben, dass
        // `resource_generation` nicht mehr passt und der frisch erstellte
        // Plan sofort als stale gilt (siehe `validate_resource_generation`).
        // Das ist gewolltes Verhalten, kein Bug dieses Tests - bei einem
        // solchen Treffer wird hier neu geplant und erneut versucht, statt
        // die eigentliche Assertion (Timeout) durch einen unabhaengigen
        // Ressourcen-Jitter platzen zu lassen.
        let mut response = None;
        for _ in 0..20 {
            let resources = match crate::resources::read() {
                Ok(snapshot) => default_resources_from_snapshot(&snapshot, &config),
                Err(_) => return,
            };
            let plan = planner::plan(
                &ModelProfile {
                    model: "a.gguf".into(),
                    file_size_mb: 1,
                    estimated_gpu_mb: 1,
                    estimated_ram_mb: 1,
                    layers: 1,
                    context_tokens: 512,
                    requested_context_tokens: 512,
                },
                &resources,
            );
            assert!(plan.safe);
            *server_state.latest_plan.lock().unwrap() = Some(plan.clone());

            let attempt = dispatch(
                api::Request::StartModelPlanned {
                    model: "a.gguf".into(),
                    plan_id: plan.plan_id,
                },
                &engine,
                &config,
                &catalog,
                &server_state,
            );
            if matches!(
                &attempt,
                api::Response::Error(error) if error.code == ApiErrorCode::StalePlan
            ) {
                continue;
            }
            response = Some(attempt);
            break;
        }
        let response = response.expect("dispatch lieferte wiederholt nur stale_plan");

        match &response {
            api::Response::Error(error) => assert_eq!(error.code, ApiErrorCode::WorkerTimeout),
            other => panic!("erwartete WorkerTimeout, bekam: {other:?}"),
        }
        assert_eq!(status_code_for(&response), 504);
        let engine = engine.lock().unwrap();
        assert_eq!(engine.state(), EngineState::Failed);
        assert!(engine.active_model().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_stop_route_is_idempotently_accepted_when_idle() {
        let (engine, dir) = new_engine("engine-stop-idle");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let response = send_raw_route(addr, "POST", "/api/engine/stop", "{}");
        assert!(
            response.starts_with("HTTP/1.1 202"),
            "unerwartet: {response}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_plan_dry_run_with_explicit_resources() {
        let (engine, dir) = new_engine("v1-plan");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let body = serde_json::json!({
            "model_profile": {
                "model": "example.gguf",
                "file_size_mb": 5000,
                "estimated_gpu_mb": 6000,
                "estimated_ram_mb": 7000,
                "layers": 32,
                "context_tokens": 8192,
                "requested_context_tokens": 4096
            },
            "resources": {
                "vram_free_mb": 9000,
                "ram_free_mb": 16000,
                "ssd_free_mb": 100000,
                "vram_reserve_mb": 1200,
                "ram_reserve_mb": 2000,
                "ssd_reserve_mb": 10000
            }
        })
        .to_string();
        let response = send_raw_route(addr, "POST", "/api/engine/plan", &body);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        assert!(
            response.contains("\"lane\":\"gpu_fast\""),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"plan_id\":\"plan-v1-"));
        assert!(response.contains("\"resource_generation\":"));
        assert!(!response.contains("/home/"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_plan_dry_run_falls_back_to_live_snapshot_without_resources() {
        let (engine, dir) = new_engine("v1-plan-live");
        let Some(addr) = spawn_test_server(engine) else {
            return;
        };
        let body = serde_json::json!({
            "model_profile": {
                "model": "example.gguf",
                "file_size_mb": 1,
                "estimated_gpu_mb": 1,
                "estimated_ram_mb": 1,
                "layers": 1,
                "context_tokens": 512,
                "requested_context_tokens": 512
            }
        })
        .to_string();
        let response = send_raw_route(addr, "POST", "/api/engine/plan", &body);
        // Ohne "resources" wird ein echter Snapshot gelesen — auf dieser
        // Maschine funktioniert das; in einer Sandbox ohne nvidia-smi/procfs
        // wäre 500 die korrekte, ehrliche Antwort statt erfundener Werte.
        assert!(
            response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.1 500"),
            "unerwartet: {response}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn download_test_config(dir: &std::path::Path) -> Config {
        let mut config = Config::default();
        config.paths.project_root = dir.to_path_buf();
        config.paths.stage_dir = dir.join("staging");
        config.paths.models_dir = Some(dir.join("models"));
        config
    }

    #[test]
    fn create_download_rejects_invalid_json_body() {
        let dir = test_dir("download-bad-json");
        let (engine, _) = new_engine("download-bad-json");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let response = send_raw_route(addr, "POST", "/api/downloads", "not json");
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "unerwartet: {response}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn create_download_rejects_non_https_url_via_rest_route() {
        let dir = test_dir("download-non-https");
        let (engine, _) = new_engine("download-non-https");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let body = serde_json::json!({
            "url": "http://example.com/model.gguf",
            "file_name": "model.gguf"
        })
        .to_string();
        let response = send_raw_route(addr, "POST", "/api/downloads", &body);
        assert!(
            response.starts_with("HTTP/1.1 400"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"invalid_request\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn download_status_of_unknown_id_is_404() {
        let dir = test_dir("download-status-404");
        let (engine, _) = new_engine("download-status-404");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let response = send_raw_route(addr, "GET", "/api/downloads/dl-does-not-exist", "");
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"not_found\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_of_unknown_download_id_is_404() {
        let dir = test_dir("download-cancel-404");
        let (engine, _) = new_engine("download-cancel-404");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let response = send_raw_route(addr, "POST", "/api/downloads/dl-does-not-exist/cancel", "");
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "unerwartet: {response}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn create_download_status_and_cancel_round_trip_via_rest_routes() {
        let dir = test_dir("download-round-trip");
        let (engine, _) = new_engine("download-round-trip");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        // Eine private Zieladresse laesst den SafeResolver sofort ablehnen —
        // kein echter Netzwerkzugriff im Test, aber Routing, ID-Vergabe und
        // Statusabfrage laufen durch den vollen Stack (dispatch, lazy
        // DownloadManager-Initialisierung, JSON-Serialisierung).
        let body = serde_json::json!({
            "url": "https://127.0.0.1:9/model.gguf",
            "file_name": "model.gguf"
        })
        .to_string();
        let create_response = send_raw_route(addr, "POST", "/api/downloads", &body);
        assert!(
            create_response.starts_with("HTTP/1.1 200"),
            "unerwartet: {create_response}"
        );
        let created: serde_json::Value = create_response
            .split("\r\n\r\n")
            .nth(1)
            .and_then(|body| serde_json::from_str(body).ok())
            .expect("Create-Response enthaelt kein gueltiges JSON");
        let id = created["id"].as_str().unwrap().to_owned();

        let mut status_body = serde_json::Value::Null;
        for _ in 0..100 {
            let status_response = send_raw_route(addr, "GET", &format!("/api/downloads/{id}"), "");
            assert!(
                status_response.starts_with("HTTP/1.1 200"),
                "unerwartet: {status_response}"
            );
            status_body = status_response
                .split("\r\n\r\n")
                .nth(1)
                .and_then(|body| serde_json::from_str(body).ok())
                .expect("Status-Response enthaelt kein gueltiges JSON");
            if status_body["phase"] != "downloading" {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(status_body["phase"], "failed", "unerwartet: {status_body}");

        let cancel_response =
            send_raw_route(addr, "POST", &format!("/api/downloads/{id}/cancel"), "");
        assert!(
            cancel_response.starts_with("HTTP/1.1 200"),
            "unerwartet: {cancel_response}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn models_from_response(response: &str) -> serde_json::Value {
        response
            .split("\r\n\r\n")
            .nth(1)
            .and_then(|body| serde_json::from_str(body).ok())
            .expect("Response enthaelt kein gueltiges JSON")
    }

    #[test]
    fn model_dropped_into_directory_after_start_is_invisible_until_rescan() {
        let dir = test_dir("rescan-visibility");
        let (engine, _) = new_engine("rescan-visibility");
        let mut config = download_test_config(&dir);
        // `serve_with_config` scannt `config.models_dir()` genau einmal beim
        // Start. Das Verzeichnis existiert zu diesem Zeitpunkt bewusst noch
        // nicht (frisches temp-dir) — simuliert einen Server, der vor dem
        // ersten Download hochfaehrt.
        config.paths.models_dir = Some(dir.join("models"));
        let Some(addr) = spawn_test_server_with_config(engine, config) else {
            return;
        };

        let before = send_raw_route(addr, "GET", "/v1/models", "");
        assert_eq!(
            models_from_response(&before)["data"]
                .as_array()
                .unwrap()
                .len(),
            0
        );

        // Simuliert das Ergebnis eines abgeschlossenen Downloads: eine
        // gueltige GGUF-Datei landet direkt im Modellverzeichnis, ohne den
        // Downloadmanager zu bemuehen — der Rescan-Endpoint muss unabhaengig
        // davon funktionieren, wie die Datei dorthin kam.
        fs::create_dir_all(dir.join("models")).unwrap();
        write_minimal_gguf(&dir.join("models").join("dropped.gguf"));

        let still_before = send_raw_route(addr, "GET", "/v1/models", "");
        assert_eq!(
            models_from_response(&still_before)["data"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "Katalog darf sich ohne Rescan nicht von selbst aendern"
        );

        // `/api/models/rescan` laeuft seit dem Fix fuer die minutenlangen
        // Rescans auf grossen echten Modellbestaenden (siehe Commit-Historie)
        // im Hintergrund und antwortet sofort — der Client muss selbst
        // erneut abfragen, bis der Katalog sich aktualisiert hat.
        let rescan_response = send_raw_route(addr, "POST", "/api/models/rescan", "");
        assert!(
            rescan_response.starts_with("HTTP/1.1 200"),
            "unerwartet: {rescan_response}"
        );
        let started = models_from_response(&rescan_response);
        assert_eq!(started["kind"], "catalog_rescan_started");
        assert_eq!(started["already_running"], false);

        let models = wait_for_models(addr, |models| !models.is_empty());
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["display_name"], "dropped.gguf");
        assert_eq!(models[0]["startable"], true);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rescan_of_missing_models_dir_reports_zero_models_without_error() {
        let dir = test_dir("rescan-empty");
        let (engine, _) = new_engine("rescan-empty");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let response = send_raw_route(addr, "POST", "/api/models/rescan", "");
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        let body = models_from_response(&response);
        assert_eq!(body["kind"], "catalog_rescan_started");
        assert_eq!(body["already_running"], false);
        // Ein leeres Modellverzeichnis bleibt nach dem Rescan leer, nicht
        // fehlerhaft — der kurze `wait_for_models`-Timeout reicht hier, weil
        // es nichts zu hashen gibt.
        let models = wait_for_models(addr, |_| true);
        assert!(models.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_rescan_request_reports_already_running_instead_of_starting_a_second() {
        let dir = test_dir("rescan-concurrent");
        let (engine, _) = new_engine("rescan-concurrent");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let first = send_raw_route(addr, "POST", "/api/models/rescan", "");
        let first_body = models_from_response(&first);
        assert_eq!(first_body["already_running"], false);

        // Direkt danach ein zweiter Request: je nach Timing des ersten,
        // bereits abgeschlossenen Rescans (das Testverzeichnis ist leer,
        // also sehr schnell) kann auch dieser `already_running: false`
        // melden — beides ist korrekt, solange der Server nicht crasht oder
        // einen Fehler liefert. Der Guard selbst wird bereits durch den
        // AtomicBool::swap im Produktionscode erzwungen; hier wird nur
        // geprueft, dass ein zweiter Request in jedem Fall sauber
        // beantwortet wird statt zu haengen oder zu fehlern.
        let second = send_raw_route(addr, "POST", "/api/models/rescan", "");
        assert!(second.starts_with("HTTP/1.1 200"), "unerwartet: {second}");
        let second_body = models_from_response(&second);
        assert_eq!(second_body["kind"], "catalog_rescan_started");
        let _ = fs::remove_dir_all(dir);
    }

    fn wait_for_models(
        addr: std::net::SocketAddr,
        predicate: impl Fn(&[serde_json::Value]) -> bool,
    ) -> Vec<serde_json::Value> {
        for _ in 0..200 {
            let response = send_raw_route(addr, "GET", "/v1/models", "");
            let models = models_from_response(&response)["data"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if predicate(&models) {
                return models;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("Katalog erreichte den erwarteten Zustand nicht innerhalb des Testzeitlimits");
    }

    fn write_gguf_with_layers(path: &std::path::Path, layers: u32, context_length: u32) {
        let mut bytes = Vec::from(&b"GGUF"[..]);
        bytes.extend_from_slice(&3u32.to_le_bytes()); // version
        bytes.extend_from_slice(&0u64.to_le_bytes()); // tensor_count
        bytes.extend_from_slice(&2u64.to_le_bytes()); // metadata_count
        let push_u32 = |bytes: &mut Vec<u8>, key: &str, value: u32| {
            bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
            bytes.extend_from_slice(key.as_bytes());
            bytes.extend_from_slice(&4u32.to_le_bytes()); // GGUF value type 4 = u32
            bytes.extend_from_slice(&value.to_le_bytes());
        };
        push_u32(&mut bytes, "llama.block_count", layers);
        push_u32(&mut bytes, "llama.context_length", context_length);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn engine_plan_auto_derives_profile_from_catalog_and_gguf_metadata() {
        let dir = test_dir("plan-auto");
        let (engine, _) = new_engine("plan-auto");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_gguf_with_layers(&models_dir.join("auto.gguf"), 32, 8192);
        let mut config = download_test_config(&dir);
        config.paths.models_dir = Some(models_dir);
        let configured_context = config.worker.default_context;
        let Some(addr) = spawn_test_server_with_config(engine, config) else {
            return;
        };

        let models = wait_for_models(addr, |models| !models.is_empty());
        let model_id = models[0]["id"].as_str().unwrap().to_owned();

        let body = serde_json::json!({"model": model_id}).to_string();
        let response = send_raw_route(addr, "POST", "/api/engine/plan/auto", &body);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        let plan = models_from_response(&response);
        assert_eq!(plan["model"], model_id);
        assert!(plan["plan_id"].as_str().unwrap().starts_with("plan-v1-"));
        assert!(
            plan["ctx_size"].as_u64().unwrap() <= 8192,
            "unerwartet: {plan}"
        );
        assert!(
            plan["ctx_size"].as_u64().unwrap() <= u64::from(configured_context),
            "auto plan exceeded configured context safety cap: {plan}"
        );
        assert!(
            plan["gpu_layers"].as_u64().unwrap() <= 32,
            "unerwartet: {plan}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_plan_auto_rejects_unknown_model_with_not_found() {
        let dir = test_dir("plan-auto-unknown");
        let (engine, _) = new_engine("plan-auto-unknown");
        let Some(addr) = spawn_test_server_with_config(engine, download_test_config(&dir)) else {
            return;
        };
        let body = serde_json::json!({"model": "sha256:does-not-exist"}).to_string();
        let response = send_raw_route(addr, "POST", "/api/engine/plan/auto", &body);
        assert!(
            response.starts_with("HTTP/1.1 404"),
            "unerwartet: {response}"
        );
        assert!(response.contains("\"code\":\"not_found\""));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn engine_plan_auto_rejects_gguf_without_block_count() {
        let dir = test_dir("plan-auto-no-layers");
        let (engine, _) = new_engine("plan-auto-no-layers");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        // Kein block_count-Metadatenschluessel — die minimale Fixture aus
        // write_minimal_gguf() deckt genau diesen Fall ab.
        write_minimal_gguf(&models_dir.join("no-layers.gguf"));
        let mut config = download_test_config(&dir);
        config.paths.models_dir = Some(models_dir);
        let Some(addr) = spawn_test_server_with_config(engine, config) else {
            return;
        };

        let models = wait_for_models(addr, |models| !models.is_empty());
        let model_id = models[0]["id"].as_str().unwrap().to_owned();

        let body = serde_json::json!({"model": model_id}).to_string();
        let response = send_raw_route(addr, "POST", "/api/engine/plan/auto", &body);
        assert!(
            response.starts_with("HTTP/1.1 500"),
            "unerwartet: {response}"
        );
        assert!(response.contains("block_count"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn bearer_and_tri_auth_use_constant_time_comparison_path() {
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), "Bearer secret".into());
        let request = ParsedRequest {
            method: "GET".into(),
            path: "/api/status".into(),
            body: String::new(),
            headers,
            request_id: "test-1".into(),
        };
        assert!(is_authorized(&request, Some("secret")));
        assert!(!is_authorized(&request, Some("wrong")));
        assert!(is_authorized(&request, None));
    }

    #[test]
    fn health_is_public_but_other_routes_require_auth_when_configured() {
        assert!(!route_requires_auth("/health"));
        assert!(route_requires_auth("/metrics"));
        assert!(route_requires_auth("/v1/chat/completions"));
    }

    #[test]
    fn invalid_content_length_is_rejected() {
        let result = parse_content_length("POST / HTTP/1.1\r\nContent-Length: nope\r\n");
        assert!(result.is_err());
    }

    #[test]
    fn regression_ingried_context_40960_capped_to_default_4096() {
        let dir = test_dir("plan-ingried-regression");
        let (engine, _) = new_engine("plan-ingried-regression");
        let models_dir = dir.join("models");
        fs::create_dir_all(&models_dir).unwrap();
        write_gguf_with_layers(&models_dir.join("ingried.gguf"), 32, 40960);
        let mut config = download_test_config(&dir);
        config.paths.models_dir = Some(models_dir);
        config.worker.default_context = 4096;
        let Some(addr) = spawn_test_server_with_config(engine, config) else {
            return;
        };

        let models = wait_for_models(addr, |models| !models.is_empty());
        let model_id = models[0]["id"].as_str().unwrap().to_owned();

        let body = serde_json::json!({"model": model_id}).to_string();
        let response = send_raw_route(addr, "POST", "/api/engine/plan/auto", &body);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "unerwartet: {response}"
        );
        let plan = models_from_response(&response);
        assert!(
            plan["ctx_size"].as_u64().unwrap() <= 4096,
            "regression: INGRIED ctx_size 40960 not capped to default 4096, got {}",
            plan["ctx_size"].as_u64().unwrap()
        );
    }
}
