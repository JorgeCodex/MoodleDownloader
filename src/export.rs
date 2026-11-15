use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use dom_query::{Document, NodeRef, Selection};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use reqwest::{RequestBuilder, header};
use serde::Serialize;
use url::Url;

use crate::{
    moodle::{self, CourseDetail, Html, Item, Redirect, ResponseOrLink, Session, SessionExpired},
    storage::{self, Saved, Staged, clean_text, component},
};

const LOCAL_URL: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'%')
    .add(b'#')
    .add(b'?')
    .add(b'&')
    .add(b'\'');

#[derive(Clone, Debug, Serialize)]
pub struct Update {
    pub level: &'static str,
    pub text: String,
}

#[derive(Clone, Default, Debug, Serialize)]
pub struct Report {
    pub saved: usize,
    pub reused: usize,
    pub bytes: u64,
    pub errors: usize,
    pub steps: usize,
}

pub struct Export<'a, F> {
    session: &'a Session,
    notify: F,
    pub report: Report,
}

impl<'a, F: Fn(Update)> Export<'a, F> {
    pub fn new(session: &'a Session, notify: F) -> Self {
        Self {
            session,
            notify,
            report: Report::default(),
        }
    }

    pub fn message(&self, level: &'static str, text: impl Into<String>) {
        (self.notify)(Update {
            level,
            text: text.into(),
        });
    }

    pub fn error(&mut self, name: &str, error: anyhow::Error) -> Result<()> {
        if error.is::<SessionExpired>() {
            return Err(error);
        }
        self.report.errors += 1;
        self.message("error", format!("{name} · {error}"));
        Ok(())
    }

    pub async fn course(
        &mut self,
        detail: &CourseDetail,
        root: &Path,
        item_filter: Option<&str>,
    ) -> Result<()> {
        let course_dir = root.join(format!(
            "[{}] {}",
            detail.course.id,
            component(&detail.course.name, 67)
        ));
        self.message("course", &detail.course.name);
        if item_filter.is_none() {
            for warning in &detail.warnings {
                self.report.errors += 1;
                self.message("error", warning);
            }
        }
        let mut matched = false;
        for section in &detail.sections {
            let section_dir = course_dir.join(format!(
                "{:02} {} [{}]",
                section.number,
                component(&section.name, 36),
                section.id
            ));
            for item in &section.items {
                let key = format!("{}/{}", section.id, item.id);
                if item_filter.is_some_and(|filter| filter != key) {
                    continue;
                }
                matched = true;
                let item_dir =
                    section_dir.join(format!("{} [{}]", component(&item.name, 43), item.id));
                self.message("item", &item.name);
                if let Err(error) = self.item(item, &item_dir).await {
                    self.error(&item.name, error)?;
                }
                self.report.steps += 1;
            }
        }
        if !matched {
            if item_filter.is_some() {
                bail!("La actividad ya no aparece en el curso. Actualiza el catálogo.");
            }
            self.message(
                "info",
                "Este curso no tiene actividades accesibles para respaldar.",
            );
        }
        Ok(())
    }

    async fn item(&mut self, item: &Item, dir: &Path) -> Result<()> {
        match item.kind.as_str() {
            "resource" => self.resource(item, dir).await,
            "folder" => self.folder(item, dir).await,
            "assign" | "page" | "quiz" | "book" => self.activity(item, dir).await,
            "contenido" => {
                self.snapshot(
                    dir,
                    "Contenido.html",
                    &item.name,
                    &item.url,
                    item.content.as_deref().unwrap_or(""),
                )
                .await
            }
            "url" => {
                let url = match self
                    .session
                    .request(self.session.get(item.url.clone()), Redirect::Link)
                    .await?
                {
                    ResponseOrLink::Link(url) => url,
                    ResponseOrLink::Response(response) => {
                        let page = moodle::read_html(response).await?;
                        let doc = Document::from(page.source.as_str());
                        doc.select(".urlworkaround a[href], #region-main a[href].external")
                            .nodes()
                            .iter()
                            .find_map(|node| moodle::resolve(&page.url, &node.attr_or("href", "")))
                            .unwrap_or(item.url.clone())
                    }
                };
                self.shortcut(dir, &item.name, &url)
            }
            _ => self.shortcut(dir, &item.name, &item.url),
        }
    }

    async fn resource(&mut self, item: &Item, dir: &Path) -> Result<()> {
        let response = self
            .session
            .response(self.session.get(item.url.clone()), Redirect::File)
            .await?;
        if is_html(&response) && filename(&response).is_none() {
            let page = moodle::read_html(response).await?;
            let doc = Document::from(page.source.as_str());
            let links = doc.select("#region-main a[href*='pluginfile.php'], #resourceobject[data], #resourceobject[src], .resourceworkaround a[href]");
            let mut assets = HashSet::new();
            for node in links.nodes() {
                let href = node
                    .attr("href")
                    .or_else(|| node.attr("data"))
                    .or_else(|| node.attr("src"));
                if let Some(url) = href.and_then(|href| moodle::resolve(&page.url, &href))
                    && self.session.owns(&url)
                    && url != page.url
                {
                    assets.insert(url);
                }
            }
            ensure!(
                !assets.is_empty(),
                "Moodle mostró una página sin un archivo descargable"
            );
            for url in assets {
                self.save_asset(dir, &url, &item.name).await?;
            }
            return Ok(());
        }
        let saved = self
            .write_response(response, dir, &item.name, false)
            .await?;
        self.record(&saved);
        Ok(())
    }

    async fn folder(&mut self, item: &Item, dir: &Path) -> Result<()> {
        let Html { url, source } = self.session.html(item.url.clone()).await?;
        let doc = Document::from(source);
        let form = doc.select_single("form[action*='download_folder.php']");
        let mut zip_error = None;
        if let Some(action) = form
            .attr("action")
            .and_then(|href| moodle::resolve(&url, &href))
            && self.session.owns(&action)
            && action.path().ends_with("/mod/folder/download_folder.php")
        {
            let values: Vec<(String, String)> = form
                .select("input[name][type=hidden], button[name][type=submit]")
                .nodes()
                .iter()
                .map(|node| {
                    (
                        node.attr_or("name", "").to_string(),
                        node.attr_or("value", "").to_string(),
                    )
                })
                .collect();
            let request = self.session.post(action).form(&values);
            match self.file(request, dir, "Carpeta.zip", true).await {
                Ok(_) => return Ok(()),
                Err(error) if error.is::<SessionExpired>() => return Err(error),
                Err(error) => zip_error = Some(error),
            }
        }
        let mut files = HashSet::new();
        for node in doc
            .select("#region-main a[href*='pluginfile.php'], .foldertree a[href*='pluginfile.php']")
            .nodes()
        {
            let Some(link) = moodle::resolve(&url, &node.attr_or("href", "")) else {
                continue;
            };
            if self.session.owns(&link) {
                files.insert(link);
            }
        }
        if files.is_empty() {
            if let Some(error) = zip_error {
                return Err(error)
                    .context("Moodle no entregó el ZIP ni mostró archivos individuales");
            }
            return self.shortcut(dir, &item.name, &item.url);
        }
        if zip_error.is_some() {
            self.message(
                "info",
                "El ZIP no está disponible. Se guardarán los archivos individuales.",
            );
        }
        for url in files {
            self.save_asset(dir, &url, "Archivo de carpeta").await?;
        }
        Ok(())
    }

    async fn activity(&mut self, item: &Item, dir: &Path) -> Result<()> {
        let page = self.session.html(item.url.clone()).await?;
        let doc = Document::from(page.source.as_str());
        let selector = match item.kind.as_str() {
            "page" => "#region-main .generalbox, #region-main .activity-description, #intro",
            "book" => ".book_content, .book-content",
            _ => "#intro, #region-main .activity-description",
        };
        let content = doc.select_single(selector).inner_html();
        let file_name = if item.kind == "assign" {
            "Instrucciones.html"
        } else {
            "Contenido.html"
        };
        self.snapshot(dir, file_name, &item.name, &page.url, &content)
            .await?;
        if item.kind == "quiz" {
            let mut attempts = HashSet::new();
            for anchor in doc
                .select("#region-main a[href], .quizattemptsummary a[href]")
                .nodes()
            {
                let Some(review) = moodle::resolve(&page.url, &anchor.attr_or("href", "")) else {
                    continue;
                };
                if self.session.owns(&review)
                    && review.path().ends_with("/mod/quiz/review.php")
                    && let Some(attempt) = moodle::query_number(&review, "attempt")
                    && attempts.insert(attempt)
                    && let Err(error) = self.review(dir, item, review, attempt).await
                {
                    self.error(&format!("Revisión del intento {attempt}"), error)?;
                }
            }
        }
        if item.kind == "book" {
            let mut chapters = HashSet::new();
            if let Some(current) = moodle::query_number(&page.url, "chapterid") {
                chapters.insert(current);
            }
            for anchor in doc
                .select(".book_toc a[href], .book-toc a[href], [data-block='book_toc'] a[href]")
                .nodes()
            {
                let Some(url) = moodle::resolve(&page.url, &anchor.attr_or("href", "")) else {
                    continue;
                };
                let id = moodle::query_number(&page.url, "id");
                if self.session.owns(&url)
                    && url.path().ends_with("/mod/book/view.php")
                    && id.is_some()
                    && moodle::query_number(&url, "id") == id
                    && let Some(chapter) = moodle::query_number(&url, "chapterid")
                    && chapters.insert(chapter)
                {
                    let name = clean_text(&anchor.text());
                    let result = async {
                        let page = self.session.html(url).await?;
                        let doc = Document::from(page.source.as_str());
                        let content = doc.select_single(selector).inner_html();
                        self.snapshot(
                            dir,
                            &format!("Capítulo {chapter}.html"),
                            &name,
                            &page.url,
                            &content,
                        )
                        .await
                    }
                    .await;
                    if let Err(error) = result {
                        self.error(&format!("Capítulo {chapter}"), error)?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn review(&mut self, dir: &Path, item: &Item, mut url: Url, attempt: u64) -> Result<()> {
        let pairs = url
            .query_pairs()
            .into_owned()
            .filter(|(key, _)| key != "showall" && key != "page")
            .collect::<Vec<_>>();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair("showall", "1");
        let page = self.session.html(url).await?;
        let doc = Document::from(page.source.as_str());
        let questions = doc.select("#region-main .que");
        ensure!(
            !questions.is_empty(),
            "Moodle no mostró preguntas para este intento"
        );
        let summary = doc.select("table.quizreviewsummary").html();
        let mut content = summary.to_string();
        for question in questions.nodes() {
            content.push_str(&question.html());
        }
        self.snapshot(
            dir,
            &format!("Revisión {attempt}.html"),
            &format!("{} · Revisión", item.name),
            &page.url,
            &content,
        )
        .await
    }

    fn shortcut(&mut self, dir: &Path, title: &str, url: &Url) -> Result<()> {
        ensure!(moodle::web_url(url), "Enlace no compatible");
        let clean_url = printable_url(url);
        let data = format!("[InternetShortcut]\r\nURL={clean_url}\r\n");
        let saved = storage::save(dir, &format!("{title}.url"), data.as_bytes())?;
        self.record(&saved);
        Ok(())
    }

    async fn snapshot(
        &mut self,
        dir: &Path,
        filename: &str,
        title: &str,
        base: &Url,
        fragment: &str,
    ) -> Result<()> {
        let doc = Document::fragment(fragment);
        // Use only the activity's published content. Submission and grading
        // tables on an assignment page are outside the selected intro.
        doc.select("script, style, svg, [aria-hidden='true'].accesshide, .accesshide, .sr-only, .visually-hidden").remove();
        let assets_dir = dir.join("Adjuntos");
        let mut assets: HashMap<Url, String> = HashMap::new();
        let mut tried = HashSet::new();
        for node in doc.select("a[href], img[src], img[data-src], video[src], audio[src], source[src], video[poster]").nodes() {
            for (attr, href) in relevant_assets(node) {
                let Some(url) = moodle::resolve(base, &href) else { continue };
                if !self.session.owns(&url) || (attr == "href" && !url.path().contains("/pluginfile.php/"))
                    || !tried.insert(url.clone())
                {
                    continue;
                }
                if let Some(file) = self.save_asset(&assets_dir, &url, "Adjunto").await? {
                    let name = utf8_percent_encode(&file.name, LOCAL_URL);
                    assets.insert(url, format!("Adjuntos/{name}"));
                }
            }
        }
        let mut body = String::new();
        render_nodes(doc.root().children(), base, &assets, &mut body, 0);
        let title = escape(title);
        let url = escape(printable_url(base).as_str());
        let source = format!(
            concat!(
                "<!doctype html><html lang=\"es\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">",
                "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file:; media-src 'self' file:; style-src 'unsafe-inline'; form-action 'none'\">",
                "<title>{}</title><style>body{{font:16px/1.6 system-ui,sans-serif;color:#14221f;background:#fff;max-width:820px;padding:0 24px;margin:40px auto}}",
                "img,video{{max-width:100%;height:auto}}table{{border-collapse:collapse}}th,td{{border:1px solid #d3dad7;padding:.4rem}}",
                "h1{{line-height:1.18}}a{{color:#00715d;overflow-wrap:anywhere}}fieldset{{margin:.6rem 0}}pre{{white-space:pre-wrap}}",
                "@media print{{body{{margin:0;padding:0;font-size:11pt}}img,tr{{break-inside:avoid}}}}</style>",
                "<main><h1>{}</h1><p><a href=\"{}\" rel=\"noreferrer\">Abrir en Moodle</a></p><hr>{}</main></html>"
            ),
            title, title, url, body
        );
        let saved = storage::save(dir, filename, source.as_bytes())?;
        self.record(&saved);
        Ok(())
    }

    async fn save_asset(&mut self, dir: &Path, url: &Url, fallback: &str) -> Result<Option<Saved>> {
        match self
            .file(self.session.get(url.clone()), dir, fallback, false)
            .await
        {
            Ok(file) => Ok(Some(file)),
            Err(error) => {
                self.error(
                    &moodle::decoded_filename(url).unwrap_or_else(|| fallback.to_owned()),
                    error,
                )?;
                Ok(None)
            }
        }
    }

    async fn file(
        &mut self,
        request: RequestBuilder,
        dir: &Path,
        fallback: &str,
        zip: bool,
    ) -> Result<Saved> {
        let response = self
            .session
            .response(
                request,
                if zip {
                    Redirect::SameSite
                } else {
                    Redirect::File
                },
            )
            .await?;
        let saved = self.write_response(response, dir, fallback, zip).await?;
        self.record(&saved);
        Ok(saved)
    }

    async fn write_response(
        &self,
        mut response: reqwest::Response,
        dir: &Path,
        fallback: &str,
        zip: bool,
    ) -> Result<Saved> {
        ensure!(
            response.status() != reqwest::StatusCode::PARTIAL_CONTENT,
            "Moodle entregó una respuesta parcial"
        );
        if is_html(&response) && (filename(&response).is_none() || zip) {
            moodle::read_html(response).await?;
            bail!("Moodle devolvió una página en lugar del archivo");
        }
        let name = filename(&response)
            .or_else(|| moodle::decoded_filename(response.url()))
            .unwrap_or_else(|| fallback.to_owned());
        let expected = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
            .or_else(|| response.content_length());
        let mut staged = Staged::new(dir)?;
        let mut magic = Vec::new();
        let mut last_update = Instant::now();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| e.without_url())
            .context("Se interrumpió la transferencia del archivo")?
        {
            if zip && magic.len() < 4 {
                magic.extend_from_slice(&chunk[..chunk.len().min(4 - magic.len())]);
            }
            staged.write(&chunk)?;
            if last_update.elapsed() >= Duration::from_millis(350) {
                self.message(
                    "transfer",
                    match expected {
                        Some(length) if length > 0 => format!(
                            "{} · {}%",
                            component(&name, 80),
                            (staged.bytes as u128 * 100 / length as u128).min(100)
                        ),
                        _ => format!(
                            "{} · {:.1} MB",
                            component(&name, 80),
                            staged.bytes as f64 / 1_000_000.0
                        ),
                    },
                );
                last_update = Instant::now();
            }
        }
        ensure!(
            expected.is_none_or(|count| count == staged.bytes),
            "El tamaño del archivo quedó incompleto"
        );
        if zip {
            ensure!(
                matches!(magic.as_slice(), b"PK\x03\x04" | b"PK\x05\x06"),
                "Moodle no devolvió un ZIP válido"
            );
        }
        let name = if zip && !name.to_ascii_lowercase().ends_with(".zip") {
            fallback
        } else {
            &name
        };
        staged.finish(name)
    }

    fn record(&mut self, saved: &Saved) {
        if saved.duplicate {
            self.report.reused += 1;
            self.message("existing", &saved.name);
        } else {
            self.report.saved += 1;
            self.report.bytes += saved.bytes;
            self.message("saved", &saved.name);
        }
    }
}

fn relevant_assets(node: &NodeRef<'_>) -> Vec<(&'static str, String)> {
    let mut assets = Vec::new();
    if let Some(href) = node.attr("href") {
        assets.push(("href", href.to_string()));
    }
    if let Some(src) = node.attr("data-src").or_else(|| node.attr("src")) {
        assets.push(("src", src.to_string()));
    }
    if let Some(poster) = node.attr("poster") {
        assets.push(("poster", poster.to_string()));
    }
    assets
}

fn is_html(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|kind| {
            let kind = kind.split(';').next().unwrap_or("").trim();
            kind.eq_ignore_ascii_case("text/html")
                || kind.eq_ignore_ascii_case("application/xhtml+xml")
        })
}

fn filename(response: &reqwest::Response) -> Option<String> {
    let header = response.headers().get(header::CONTENT_DISPOSITION)?;
    match std::str::from_utf8(header.as_bytes()) {
        Ok(value) => disposition_filename(value),
        Err(_) => {
            let (value, _, _) = encoding_rs::WINDOWS_1252.decode(header.as_bytes());
            disposition_filename(&value)
        }
    }
}

fn disposition_filename(value: &str) -> Option<String> {
    let mut quoted = false;
    let mut escaped = false;
    let parts = value.split(|character| {
        if escaped {
            escaped = false;
            return false;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => return true,
            _ => {}
        }
        false
    });
    let mut ordinary = None;
    for (key, value) in parts.skip(1).filter_map(|part| part.trim().split_once('=')) {
        let value = value.trim().trim_matches('"');
        if key.trim().eq_ignore_ascii_case("filename*")
            && let Some((charset, tail)) = value.split_once('\'')
            && let Some((_, encoded)) = tail.split_once('\'')
            && let Some(encoding) = encoding_rs::Encoding::for_label(charset.as_bytes())
        {
            let bytes = percent_encoding::percent_decode_str(encoded).collect::<Vec<_>>();
            let (name, _, bad) = encoding.decode(&bytes);
            if !bad && !name.trim().is_empty() {
                return Some(name.into_owned());
            }
        }
        if key.trim().eq_ignore_ascii_case("filename") && !value.is_empty() {
            ordinary = Some(value.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
    }
    ordinary
}

fn printable_url(url: &Url) -> Url {
    let mut output = url.clone();
    if url
        .query_pairs()
        .any(|(key, _)| key.eq_ignore_ascii_case("sesskey"))
    {
        let pairs = url
            .query_pairs()
            .into_owned()
            .filter(|(key, _)| !key.eq_ignore_ascii_case("sesskey"))
            .collect::<Vec<_>>();
        output.set_query(None);
        if !pairs.is_empty() {
            output.query_pairs_mut().extend_pairs(pairs);
        }
    }
    output
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn render_nodes(
    nodes: Vec<NodeRef<'_>>,
    base: &Url,
    assets: &HashMap<Url, String>,
    out: &mut String,
    depth: usize,
) {
    if depth >= 192 {
        return;
    }
    for node in nodes {
        if node.is_text() {
            out.push_str(&escape(&node.text()));
            continue;
        }
        let tag = node.node_name().unwrap_or_default();
        let tag: &str = &tag;
        match tag {
            "script" | "style" | "noscript" | "template" | "svg" | "button" => continue,
            "a" => {
                let url = node
                    .attr("href")
                    .and_then(|href| moodle::resolve(base, &href));
                if let Some(url) = url {
                    let href = assets
                        .get(&url)
                        .cloned()
                        .unwrap_or_else(|| printable_url(&url).to_string());
                    let _ = write!(out, "<a href=\"{}\" rel=\"noreferrer\">", escape(&href));
                    render_nodes(node.children(), base, assets, out, depth + 1);
                    out.push_str("</a>");
                } else {
                    render_nodes(node.children(), base, assets, out, depth + 1);
                }
            }
            "img" | "iframe" | "embed" | "object" | "source" => {
                let attr = if tag == "object" { "data" } else { "src" };
                let source = node
                    .attr("data-src")
                    .or_else(|| node.attr(attr))
                    .and_then(|src| moodle::resolve(base, &src));
                if let Some(url) = source {
                    let local = assets.get(&url);
                    let caption = node
                        .attr("alt")
                        .or_else(|| node.attr("title"))
                        .unwrap_or_else(|| "Abrir recurso multimedia".into());
                    match (tag, local) {
                        ("img", Some(local)) => {
                            let _ = write!(
                                out,
                                "<img src=\"{}\" alt=\"{}\">",
                                escape(local),
                                escape(&caption)
                            );
                        }
                        ("source", Some(local)) => {
                            let _ = write!(out, "<source src=\"{}\">", escape(local));
                        }
                        ("source", None) => {}
                        _ => {
                            let _ = write!(
                                out,
                                "<p><a href=\"{}\" rel=\"noreferrer\">{}</a></p>",
                                escape(printable_url(&url).as_str()),
                                escape(&caption)
                            );
                        }
                    }
                }
            }
            "video" | "audio" => {
                let _ = write!(out, "<{tag} controls preload=\"metadata\"");
                for key in ["src", "poster"] {
                    if let Some(url) = node.attr(key).and_then(|src| moodle::resolve(base, &src)) {
                        if let Some(local) = assets.get(&url) {
                            let _ = write!(out, " {key}=\"{}\"", escape(local));
                        }
                    }
                }
                out.push('>');
                render_nodes(node.children(), base, assets, out, depth + 1);
                let _ = write!(out, "</{tag}>");
                for child in std::iter::once(node).chain(
                    Selection::from(node)
                        .select("source[src]")
                        .nodes()
                        .iter()
                        .cloned(),
                ) {
                    if let Some(url) = child
                        .attr("src")
                        .and_then(|src| moodle::resolve(base, &src))
                        && !assets.contains_key(&url)
                    {
                        let _ = write!(
                            out,
                            "<p><a href=\"{}\" rel=\"noreferrer\">Abrir multimedia</a></p>",
                            escape(printable_url(&url).as_str())
                        );
                    }
                }
            }
            "input" => {
                let kind = node.attr_or("type", "text").to_ascii_lowercase();
                if matches!(kind.as_str(), "radio" | "checkbox") {
                    let checked = if node.has_attr("checked") {
                        " checked"
                    } else {
                        ""
                    };
                    let _ = write!(out, "<input type=\"{kind}\"{checked} disabled>");
                } else if matches!(kind.as_str(), "text" | "number")
                    && let Some(value) = node.attr("value")
                {
                    let _ = write!(out, "<span>{}</span>", escape(&value));
                }
            }
            "br" | "hr" => {
                let _ = write!(out, "<{tag}>");
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "section" | "div" | "span" | "ul"
            | "ol" | "li" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "th" | "td"
            | "blockquote" | "pre" | "code" | "strong" | "em" | "b" | "i" | "u" | "sup" | "sub"
            | "dl" | "dt" | "dd" | "fieldset" | "legend" | "figure" | "figcaption" | "label"
            | "select" | "option" | "textarea" => {
                let _ = write!(out, "<{tag}");
                if matches!(tag, "select" | "textarea") {
                    out.push_str(" disabled");
                }
                if tag == "option" && node.has_attr("selected") {
                    out.push_str(" selected");
                }
                for attribute in ["colspan", "rowspan", "start"] {
                    if let Some(value) = node
                        .attr(attribute)
                        .and_then(|value| value.parse::<u16>().ok())
                    {
                        let _ = write!(out, " {attribute}=\"{value}\"");
                    }
                }
                out.push('>');
                render_nodes(node.children(), base, assets, out, depth + 1);
                let _ = write!(out, "</{tag}>");
            }
            _ => render_nodes(node.children(), base, assets, out, depth + 1),
        }
    }
}

pub fn target_path(input: &str) -> Result<PathBuf> {
    let path = PathBuf::from(input.trim());
    ensure!(
        path.is_absolute(),
        "Selecciona una carpeta de destino con ruta absoluta"
    );
    ensure!(
        !input.contains('\0'),
        "La carpeta de destino contiene caracteres inválidos"
    );
    Ok(path)
}
