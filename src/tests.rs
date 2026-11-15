use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use reqwest::{Request, Response, ResponseBuilderExt, header};
use serde_json::json;
use uuid::Uuid;

use crate::{
    export::Export,
    moodle::{Course, Session, SessionExpired},
    storage::{self, Staged},
};

const ADDRESS: &str = "https://aula.example.test/campus/";

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("moodle-test-{}", Uuid::new_v4())))
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Mock {
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn start() -> Self {
        Self::default()
    }

    fn session(&self) -> Session {
        self.with_cookie("test-only")
    }

    fn with_cookie(&self, cookie: &str) -> Session {
        let requests = self.requests.clone();
        Session::new(ADDRESS, cookie)
            .unwrap()
            .with_fixture(Arc::new(move |request| {
                requests.lock().unwrap().push(request.url().to_string());
                respond(request)
            }))
    }

    fn seen(&self, needle: &str) -> bool {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains(needle))
    }
}

fn respond(request: &Request) -> Response {
    let url = request.url();
    let response = wry::http::Response::builder().url(url.clone());
    let cookie = request
        .headers()
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok());
    if url.host_str() == Some("cdn.example.test") {
        assert!(cookie.is_none());
        return response
            .header(header::CONTENT_TYPE, "application/pdf")
            .body("%PDF-DUMMY".to_owned())
            .unwrap()
            .into();
    }
    if cookie != Some("MoodleSession=test-only") {
        return response
            .status(302)
            .header(header::LOCATION, "/campus/login/index.php")
            .body(String::new())
            .unwrap()
            .into();
    }
    let path = url.path().strip_prefix("/campus/").unwrap();
    let query = url.query().unwrap_or("");
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .unwrap_or_default();
    let mut kind = "text/html; charset=utf-8";
    let mut filename = None;
    let mut length = None;
    let payload = match (path, query) {
        ("my/" | "my/courses.php", _) => r#"<script>M.cfg = { "sesskey" : "Test123" }</script>"#.into(),
        ("lib/ajax/service.php", _) => {
            kind = "application/json";
            let body: serde_json::Value = serde_json::from_slice(body).unwrap();
            let offset = body[0]["args"]["offset"].as_u64().unwrap();
            if offset == 0 {
                json!([{"data":{"courses":[{"id":42,"fullname":"Diseño &amp; CAD"}],"nextoffset":1}}]).to_string()
            } else {
                json!([{"data":{"courses":[{"id":43,"fullname":"Gestión de redes"}],"nextoffset":-1}}]).to_string()
            }
        }
        ("course/view.php", "id=42") => r#"<main id="region-main"><div class="course-content"><ul>
            <li id="section-0" data-sectionid="99"><h3 class="sectionname">General</h3><ul>
              <li class="activity modtype_resource" id="module-501"><a href="../mod/resource/view.php?id=501"><span class="instancename">Plano <span class="accesshide">Archivo</span></span></a></li>
              <li class="activity modtype_resource" id="module-502"><a href="../mod/resource/view.php?id=502"><span class="instancename">Cortado</span></a></li>
              <li class="activity modtype_quiz" id="module-503"><a href="../mod/quiz/view.php?id=503">Prueba de CAD</a></li>
              <li class="activity modtype_folder" id="module-504"><a href="../mod/folder/view.php?id=504">Carpeta de planos</a></li>
            </ul></li>
            <li id="section-1" data-sectionid="100"><h3 class="sectionname"><a href="section.php?id=100">Proyectos</a></h3></li>
            </ul></div></main>"#.into(),
        ("course/section.php", "id=100") => r#"<main id="region-main"><section class="course-section" data-sectionid="100" data-number="1"><h3 class="sectionname">Proyectos</h3>
            <div data-for="cmitem" data-id="505"><a href="../mod/page/view.php?id=505">Introducción</a></div>
            </section></main>"#.into(),
        ("mod/resource/view.php", "id=501") => {
            return response.status(302).header(header::LOCATION, "https://cdn.example.test/Dise%C3%B1o.pdf")
                .body(String::new()).unwrap().into();
        }
        ("mod/resource/view.php", "id=502") => {
            kind = "application/octet-stream";
            filename = Some("attachment; filename=cortado.bin");
            length = Some(1000);
            "incompleto".into()
        }
        ("mod/quiz/view.php", "id=503") => r#"<main id="region-main"><div id="intro">Instrucciones</div>
          <a href="review.php?attempt=8&amp;page=0">Revisión</a></main>"#.into(),
        ("mod/quiz/review.php", "attempt=8&showall=1") => r#"<main id="region-main"><div class="que"><p>Pregunta del intento</p>
          <input type="radio" checked onclick="alert(1)"><img src="/campus/pluginfile.php/8/img.png">
          <script>alert(1)</script></div></main>"#.into(),
        ("pluginfile.php/8/img.png", _) => {
            kind = "image/png";
            "test-image".into()
        }
        ("mod/folder/view.php", "id=504") => r#"<main id="region-main"><form action="download_folder.php">
          <input type="hidden" name="sesskey" value="Test123"></form></main>"#.into(),
        ("mod/folder/download_folder.php", _) => {
            assert_eq!(body, b"sesskey=Test123");
            kind = "application/zip";
            filename = Some("attachment; filename*=UTF-8''planos.zip");
            "PK\u{3}\u{4}zip-sintético".into()
        }
        _ => return response.status(404).body(String::new()).unwrap().into(),
    };
    let mut response = response.header(header::CONTENT_TYPE, kind);
    if let Some(filename) = filename {
        response = response.header(header::CONTENT_DISPOSITION, filename);
    }
    if let Some(length) = length {
        response = response.header(header::CONTENT_LENGTH, length);
    }
    response.body(payload).unwrap().into()
}

fn find_files(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().unwrap().is_dir() {
                files.extend(find_files(&entry.path()));
            } else {
                files.push(entry.path());
            }
        }
    }
    files
}

#[tokio::test]
async fn catalog_paginates_and_handles_section_pages_on_the_configured_site() {
    let mock = Mock::start();
    let session = mock.session();
    let (courses, warning) = session.courses().await.unwrap();
    assert!(warning.is_none());
    assert_eq!(courses.len(), 2);
    assert_eq!(courses[0].name, "Diseño & CAD");
    let detail = session.course(courses[0].clone()).await.unwrap();
    assert_eq!(detail.sections.len(), 2);
    assert_eq!(detail.sections[0].items.len(), 4);
    assert_eq!(detail.sections[0].items[0].name, "Plano");
    assert_eq!(detail.sections[1].items[0].id, "m505");
}

#[tokio::test]
async fn selecting_one_activity_only_downloads_that_activity() {
    let mock = Mock::start();
    let session = mock.session();
    let detail = session
        .course(Course {
            id: 42,
            name: "CAD".into(),
        })
        .await
        .unwrap();
    let temp = Temp::new();
    let mut export = Export::new(&session, |_| {});
    export
        .course(&detail, &temp.0, Some("99/m501"))
        .await
        .unwrap();
    assert_eq!(export.report.saved, 1);
    assert!(!mock.seen("id=502"));
    assert!(!mock.seen("id=503"));
    let files = find_files(&temp.0);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_name().unwrap(), "Diseño.pdf");
    assert_eq!(fs::read(&files[0]).unwrap(), b"%PDF-DUMMY");
    export
        .course(&detail, &temp.0, Some("99/m501"))
        .await
        .unwrap();
    assert_eq!(export.report.reused, 1);
}

#[tokio::test]
async fn incomplete_transfer_does_not_block_the_rest_of_a_course() {
    let mock = Mock::start();
    let session = mock.session();
    let detail = session
        .course(Course {
            id: 42,
            name: "CAD".into(),
        })
        .await
        .unwrap();
    let temp = Temp::new();
    let mut export = Export::new(&session, |_| {});
    export.course(&detail, &temp.0, None).await.unwrap();
    assert_eq!(export.report.errors, 2); // Partial transfer and the fixture's missing page.
    let files = find_files(&temp.0);
    assert!(!files.iter().any(|file| matches!(
        file.extension().and_then(|v| v.to_str()),
        Some("part" | "bin")
    )));
    let review = files
        .iter()
        .find(|path| path.file_name().unwrap() == "Revisión 8.html")
        .unwrap();
    let html = fs::read_to_string(review).unwrap();
    assert!(html.contains("Pregunta del intento"));
    assert!(html.contains("checked disabled"));
    assert!(html.contains("Adjuntos/img.png"));
    assert!(!html.contains("alert(1)"));
    assert!(mock.seen("attempt=8&showall=1"));
    assert!(
        files
            .iter()
            .any(|path| path.file_name().unwrap() == "planos.zip")
    );
}

#[tokio::test]
async fn expired_session_is_distinguished_from_empty_catalog() {
    let mock = Mock::start();
    let session = mock.with_cookie("expired");
    let error = session.courses().await.unwrap_err();
    assert!(error.is::<SessionExpired>());
    assert!(!mock.seen("/login/"));
}

#[test]
fn safe_names_and_atomic_writes_keep_existing_data() {
    let temp = Temp::new();
    assert_eq!(storage::component("NUL.txt", 60), "_NUL.txt");
    assert_eq!(storage::component("TÃ©cnica", 60), "Técnica");
    assert_eq!(storage::component("Válido 🙃", 8), "Válido");
    let first = storage::save(&temp.0, "clase.zip", b"first").unwrap();
    let second = storage::save(&temp.0, "clase.zip", b"second").unwrap();
    let duplicate = storage::save(&temp.0, "clase.zip", b"second").unwrap();
    assert_ne!(first.name, second.name);
    assert_eq!(second.name, duplicate.name);
    assert!(duplicate.duplicate);
    assert_eq!(fs::read(temp.0.join(first.name)).unwrap(), b"first");
    {
        let mut interrupted = Staged::new(&temp.0).unwrap();
        interrupted.write(b"partial").unwrap();
    }
    assert_eq!(find_files(&temp.0).len(), 2);
}
