#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod export;
mod moodle;
mod storage;
#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::Path,
    thread,
};

use anyhow::{Context, Result, ensure};
use export::{Export, Report, Update};
use moodle::{Course, CourseDetail, Session, SessionExpired};
use serde::Deserialize;
use serde_json::{Value, json};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::{IconExtWindows, WindowBuilderExtWindows},
    window::{Icon, Window, WindowBuilder},
};
use tokio::sync::oneshot;
use url::Url;
use windows_sys::Win32::UI::{
    Shell::ShellExecuteW,
    WindowsAndMessaging::{MB_ICONERROR, MessageBoxW, SW_SHOWNORMAL},
};
use wry::{
    WebContext, WebView, WebViewBuilder,
    http::{Response, header},
};

const INDEX: &str = "http://moodle.localhost/";
const APP_URL: &str = "moodle://localhost/";

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Command {
    Connect {
        address: String,
        cookie: String,
    },
    Course {
        id: u64,
    },
    Download {
        ids: Vec<u64>,
        item: Option<String>,
        target: String,
    },
    Cancel,
    ChooseFolder {
        target: String,
    },
    OpenFolder {
        target: String,
    },
    OpenUrl {
        url: String,
    },
    Disconnect,
}

enum Output {
    Catalog(Session, Vec<Course>, Option<String>),
    Detail(CourseDetail),
    Download(Report),
}

enum UiEvent {
    Command(Command),
    Progress(Update),
    Finished(Option<Result<Output>>),
}

#[derive(Default)]
struct App {
    session: Option<Session>,
    courses: Vec<Course>,
    details: HashMap<u64, CourseDetail>,
    job: Option<Job>,
    closing: bool,
}

struct Job {
    mode: &'static str,
    cancel: Option<oneshot::Sender<()>>,
}

impl App {
    fn command(
        &mut self,
        command: Command,
        window: &Window,
        web: &WebView,
        proxy: &EventLoopProxy<UiEvent>,
    ) -> Result<()> {
        if self.closing {
            return Ok(());
        }
        match command {
            Command::Cancel => self.cancel(web),
            Command::OpenFolder { target } => {
                let path = export::target_path(&target)?;
                ensure!(path.is_dir(), "La carpeta todavía no existe");
                open_external(path.as_os_str())?;
            }
            Command::OpenUrl { url } => {
                let url = Url::parse(&url).context("El enlace no es válido")?;
                ensure!(moodle::web_url(&url), "El enlace debe usar HTTP o HTTPS");
                open_external(OsStr::new(url.as_str()))?;
            }
            _ if self.job.is_some() => {}
            Command::ChooseFolder { target } => {
                let mut dialog = rfd::FileDialog::new()
                    .set_parent(window)
                    .set_title("Carpeta de descargas de Moodle");
                if let Ok(path) = export::target_path(&target)
                    && path.is_dir()
                {
                    dialog = dialog.set_directory(path);
                }
                if let Some(folder) = dialog.pick_folder() {
                    send(
                        web,
                        json!({"type": "target", "target": folder.to_string_lossy()}),
                    );
                }
            }
            Command::Disconnect => {
                self.session = None;
                self.courses.clear();
                self.details.clear();
                send(web, json!({"type": "disconnected"}));
            }
            Command::Connect { address, cookie } => {
                self.start("connect", proxy, web, move |_| async move {
                    let session = Session::new(&address, &cookie)?;
                    drop(cookie);
                    let (courses, warning) = session.courses().await?;
                    Ok(Output::Catalog(session, courses, warning))
                })?;
            }
            Command::Course { id } => {
                if let Some(detail) = self.details.get(&id) {
                    send(web, json!({"type":"detail", "detail":detail}));
                    return Ok(());
                }
                let course = self
                    .courses
                    .iter()
                    .find(|course| course.id == id)
                    .cloned()
                    .context("El curso no está en el catálogo")?;
                let session = self
                    .session
                    .clone()
                    .context("Primero conecta una sesión de Moodle")?;
                self.start("course", proxy, web, move |_| async move {
                    Ok(Output::Detail(session.course(course).await?))
                })?;
            }
            Command::Download { ids, item, target } => {
                let root = export::target_path(&target)?;
                ensure!(
                    !ids.is_empty() && (item.is_none() || ids.len() == 1),
                    "Selecciona el curso o la actividad que quieres respaldar"
                );
                let ids: HashSet<_> = ids.into_iter().collect();
                let selected: Vec<_> = self
                    .courses
                    .iter()
                    .filter(|course| ids.contains(&course.id))
                    .cloned()
                    .map(|course| {
                        let detail = self.details.get(&course.id).cloned();
                        (course, detail)
                    })
                    .collect();
                ensure!(
                    ids.len() == selected.len(),
                    "Algún curso seleccionado ya no está en el catálogo"
                );
                let session = self
                    .session
                    .clone()
                    .context("Primero conecta una sesión de Moodle")?;
                self.start("download", proxy, web, move |proxy| async move {
                    let mut export = Export::new(&session, |update| {
                        let _ = proxy.send_event(UiEvent::Progress(update));
                    });
                    let count = selected.len();
                    for (index, (course, detail)) in selected.into_iter().enumerate() {
                        export.message(
                            "batch",
                            format!("Curso {} de {count} · {}", index + 1, course.name),
                        );
                        let name = course.name.clone();
                        let result = match detail {
                            Some(detail) => Ok(detail),
                            None => session.course(course).await,
                        };
                        match result {
                            Ok(detail) => {
                                if let Err(error) =
                                    export.course(&detail, &root, item.as_deref()).await
                                {
                                    export.error(&name, error)?;
                                }
                            }
                            Err(error) => export.error(&name, error)?,
                        }
                    }
                    Ok(Output::Download(export.report))
                })?;
            }
        }
        Ok(())
    }

    fn start<F, Work>(
        &mut self,
        mode: &'static str,
        proxy: &EventLoopProxy<UiEvent>,
        web: &WebView,
        run: F,
    ) -> Result<()>
    where
        F: FnOnce(EventLoopProxy<UiEvent>) -> Work + Send + 'static,
        Work: Future<Output = Result<Output>> + 'static,
    {
        let (cancel, stop) = oneshot::channel();
        let proxy = proxy.clone();
        thread::Builder::new()
            .name(format!("moodle-{mode}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    match runtime {
                        Ok(runtime) => runtime.block_on(async {
                            tokio::select! {
                                biased;
                                _ = stop => None,
                                result = run(proxy.clone()) => Some(result),
                            }
                        }),
                        Err(error) => Some(Err(error.into())),
                    }
                }))
                .unwrap_or_else(|_| {
                    Some(Err(anyhow::anyhow!(
                        "La operación se interrumpió inesperadamente."
                    )))
                });
                let _ = proxy.send_event(UiEvent::Finished(result));
            })
            .context("No se pudo iniciar la operación")?;
        self.job = Some(Job {
            mode,
            cancel: Some(cancel),
        });
        send(web, json!({"type":"started", "mode":mode}));
        Ok(())
    }

    fn cancel(&mut self, web: &WebView) {
        if let Some(cancel) = self.job.as_mut().and_then(|job| job.cancel.take()) {
            let _ = cancel.send(());
            send(web, json!({"type":"cancelling"}));
        }
    }

    fn finish(&mut self, result: Option<Result<Output>>, web: &WebView) {
        let mode = self.job.take().map(|job| job.mode);
        match result {
            Some(Ok(Output::Catalog(session, courses, warning))) => {
                self.session = Some(session);
                self.courses = courses;
                self.details.clear();
                send(
                    web,
                    json!({"type":"catalog", "courses":self.courses, "warning":warning}),
                );
            }
            Some(Ok(Output::Detail(detail))) => {
                send(web, json!({"type":"detail", "detail":detail}));
                self.details.insert(detail.course.id, detail);
            }
            Some(Ok(Output::Download(report))) => {
                send(web, json!({"type":"report", "report":report}))
            }
            Some(Err(error)) => {
                let expired = error.is::<SessionExpired>();
                if expired {
                    self.session = None;
                    self.details.clear();
                }
                send(
                    web,
                    json!({"type":"failed", "mode":mode, "text":error.to_string(), "expired":expired}),
                );
            }
            None => send(web, json!({"type":"cancelled", "mode":mode})),
        }
    }
}

fn send(web: &WebView, message: Value) {
    let script = format!("window.receiveMoodle({message})");
    let _ = web.evaluate_script(&script);
}

fn open_external(target: &OsStr) -> Result<()> {
    let verb = storage::wide(OsStr::new("open"));
    let target = storage::wide(target);
    // ShellExecuteW uses the registered app for the supplied path or URL.
    // Pass the full target as a single argument without a command interpreter.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    ensure!(
        result as isize > 32,
        "Windows no pudo abrir el enlace o la carpeta"
    );
    Ok(())
}

fn run() -> Result<()> {
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    let icon = Icon::from_resource(101, None).context("No se pudo cargar el icono de la app")?;
    let window = WindowBuilder::new()
        .with_title("Moodle Downloader")
        .with_window_icon(Some(icon.clone()))
        .with_taskbar_icon(Some(icon))
        .with_inner_size(LogicalSize::new(1260., 820.))
        .with_min_inner_size(LogicalSize::new(900., 640.))
        .build(&event_loop)?;
    let proxy = event_loop.create_proxy();
    let inbox = proxy.clone();
    let profile = std::env::var_os("LOCALAPPDATA")
        .map(|path| Path::new(&path).join("MoodleDownloader").join("WebView"));
    let mut context = WebContext::new(profile);
    let web = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(APP_URL)
        .with_background_color((241, 246, 243, 255))
        .with_custom_protocol("moodle".into(), |_, request| {
            let (status, mime, body): (_, _, &[u8]) = match request.uri().path() {
                "/" => (200, "text/html; charset=utf-8", include_bytes!("../ui/index.html")),
                "/app.css" => (200, "text/css; charset=utf-8", include_bytes!("../ui/app.css")),
                "/app.js" => (200, "text/javascript; charset=utf-8", include_bytes!("../ui/app.js")),
                "/logo.png" => (200, "image/png", include_bytes!("../assets/logo.png")),
                _ => (404, "text/plain; charset=utf-8", "Recurso no encontrado".as_bytes()),
            };
            Response::builder().status(status).header(header::CONTENT_TYPE, mime)
                .header("X-Content-Type-Options", "nosniff")
                .header("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'")
                .body(body.into()).unwrap()
        })
        .with_navigation_handler(|url| url == INDEX || url == APP_URL)
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_drag_drop_handler(|_| true)
        .with_ipc_handler(move |request| {
            if matches!(request.uri().to_string().as_str(), INDEX | APP_URL)
                && let Ok(command) = serde_json::from_str(request.body())
            {
                let _ = inbox.send_event(UiEvent::Command(command));
            }
        })
        .build(&window)
        .context("No se pudo abrir la interfaz. Verifica que Microsoft Edge WebView2 esté instalado.")?;
    let mut state = App::default();
    event_loop.run(move |event, _, control| {
        *control = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if state.job.is_some() {
                    state.closing = true;
                    state.cancel(&web);
                } else {
                    *control = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UiEvent::Command(command)) => {
                if let Err(error) = state.command(command, &window, &web, &proxy) {
                    send(&web, json!({"type":"notice", "text":error.to_string()}));
                }
            }
            Event::UserEvent(UiEvent::Progress(update)) => {
                send(&web, json!({"type":"progress", "update":update}));
            }
            Event::UserEvent(UiEvent::Finished(result)) => {
                if state.closing {
                    *control = ControlFlow::Exit;
                } else {
                    state.finish(result, &web);
                }
            }
            _ => {}
        }
    })
}

fn main() {
    if let Err(error) = run() {
        let title = storage::wide(OsStr::new("Moodle Downloader"));
        let text = storage::wide(OsStr::new(&error.to_string()));
        // The message and title are valid NUL-terminated UTF-16 strings.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_ICONERROR,
            );
        }
    }
}
