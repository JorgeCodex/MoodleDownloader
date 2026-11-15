use std::{
    collections::{BTreeMap, HashSet},
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use dom_query::{Document, NodeRef};
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{Client, RequestBuilder, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::storage::{clean_text, short_hash};

const COURSE_METHOD: &str = "core_course_get_enrolled_courses_by_timeline_classification";
const PAGE_LIMIT: usize = 64;
const MAX_HTML: usize = 24 * 1024 * 1024;
static SESSION_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"["']?\bsesskey["']?\s*:\s*["']([A-Za-z0-9]+)["']"#).unwrap());

#[derive(Clone, Debug, Serialize)]
pub struct Course {
    pub id: u64,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CourseDetail {
    pub course: Course,
    pub sections: Vec<Section>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Section {
    pub number: u64,
    pub id: String,
    pub name: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub url: Url,
    #[serde(skip)]
    pub content: Option<String>,
}

#[derive(Debug)]
pub struct SessionExpired;

impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "Moodle solicita iniciar sesión de nuevo. Pega una cookie MoodleSession vigente.",
        )
    }
}

impl std::error::Error for SessionExpired {}

#[derive(Clone)]
pub struct Session {
    base: Url,
    cookie: header::HeaderValue,
    client: Client,
    #[cfg(test)]
    fixture: Option<Fixture>,
}

#[cfg(test)]
type Fixture = std::sync::Arc<dyn Fn(&reqwest::Request) -> Response + Send + Sync>;

pub enum ResponseOrLink {
    Response(Response),
    Link(Url),
}

#[derive(Clone, Copy)]
pub enum Redirect {
    SameSite,
    File,
    Link,
}

pub struct Html {
    pub url: Url,
    pub source: String,
}

#[derive(Deserialize)]
struct CourseReply {
    courses: Vec<CourseEntry>,
    nextoffset: Option<i64>,
}

#[derive(Deserialize)]
struct CourseEntry {
    id: u64,
    fullname: Option<String>,
    shortname: Option<String>,
}

impl Session {
    pub fn new(address: &str, cookie: &str) -> Result<Self> {
        let address = address.trim();
        let address = if address.contains("://") {
            address.into()
        } else {
            format!("https://{address}")
        };
        let mut base = Url::parse(&address).context("Escribe una dirección de Moodle válida")?;
        ensure!(is_secure(&base), "El aula debe usar HTTPS");
        ensure!(
            web_url(&base) && base.query().is_none() && base.fragment().is_none(),
            "Usa la dirección base del aula, sin credenciales ni parámetros"
        );
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let cookie = cookie
            .trim()
            .strip_prefix("MoodleSession=")
            .unwrap_or(cookie.trim());
        ensure!(
            !cookie.is_empty()
                && cookie.len() <= 4096
                && cookie.bytes().all(|b| {
                    matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
                }),
            "Pega el valor de MoodleSession de una sesión iniciada"
        );
        let mut cookie = header::HeaderValue::from_str(&format!("MoodleSession={cookie}"))?;
        cookie.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(12))
            .read_timeout(Duration::from_secs(25))
            .user_agent(concat!("MoodleDownloader/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("No se pudo inicializar la conexión HTTPS")?;
        Ok(Self {
            base,
            cookie,
            client,
            #[cfg(test)]
            fixture: None,
        })
    }

    #[cfg(test)]
    pub fn with_fixture(mut self, fixture: Fixture) -> Self {
        self.fixture = Some(fixture);
        self
    }

    async fn execute(&self, request: reqwest::Request) -> Result<Response> {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return Ok(fixture(&request));
        }
        self.client.execute(request).await.map_err(|error| {
            let timed_out = error.is_timeout();
            anyhow::Error::new(error.without_url()).context(if timed_out {
                "Moodle tardó demasiado en responder"
            } else {
                "No se pudo conectar con el servidor"
            })
        })
    }

    pub fn endpoint(&self, path: &str) -> Url {
        self.base.join(path).expect("Ruta interna relativa")
    }

    pub fn owns(&self, url: &Url) -> bool {
        url.origin() == self.base.origin() && web_url(url)
    }

    pub fn get(&self, url: Url) -> RequestBuilder {
        self.client.get(url)
    }

    pub fn post(&self, url: Url) -> RequestBuilder {
        self.client.post(url)
    }

    pub async fn request(
        &self,
        builder: RequestBuilder,
        policy: Redirect,
    ) -> Result<ResponseOrLink> {
        let mut request = builder.build()?;
        ensure!(
            self.owns(request.url()),
            "La petición inicial no pertenece al aula"
        );
        let mut external = false;
        for _ in 0..10 {
            let current = request.url().clone();
            let method = request.method().clone();
            let replay = request.try_clone();
            request.headers_mut().remove(header::COOKIE);
            if !external {
                if login_path(&current) {
                    return Err(SessionExpired.into());
                }
                request
                    .headers_mut()
                    .insert(header::COOKIE, self.cookie.clone());
            }
            let response = self.execute(request).await?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .context("El servidor devolvió una redirección sin destino")?;
                let mut destination = current
                    .join(location)
                    .context("Redirección de Moodle inválida")?;
                ensure!(
                    web_url(&destination),
                    "Moodle devolvió un enlace no compatible"
                );
                destination.set_fragment(None);
                if !self.owns(&destination) {
                    if matches!(policy, Redirect::Link) {
                        return Ok(ResponseOrLink::Link(destination));
                    }
                    if matches!(policy, Redirect::SameSite) {
                        return Err(SessionExpired.into());
                    }
                    ensure!(is_secure(&destination), "Redirección de archivo sin HTTPS");
                    external = true;
                }
                let preserve_method =
                    method != reqwest::Method::GET && matches!(status.as_u16(), 307 | 308);
                request = if preserve_method {
                    ensure!(
                        !external,
                        "El formulario de Moodle se redirigió a otro sitio"
                    );
                    let mut cloned =
                        replay.context("No se pudo repetir el formulario de Moodle")?;
                    *cloned.url_mut() = destination;
                    cloned
                } else {
                    self.get(destination).build()?
                };
                continue;
            }
            if status == StatusCode::UNAUTHORIZED && !external {
                return Err(SessionExpired.into());
            }
            ensure!(
                status.is_success(),
                "El servidor devolvió HTTP {}",
                status.as_u16()
            );
            return Ok(ResponseOrLink::Response(response));
        }
        bail!("El servidor devolvió demasiadas redirecciones")
    }

    pub async fn html(&self, url: Url) -> Result<Html> {
        let response = self.response(self.get(url), Redirect::SameSite).await?;
        read_html(response).await
    }

    pub async fn response(&self, builder: RequestBuilder, policy: Redirect) -> Result<Response> {
        match self.request(builder, policy).await? {
            ResponseOrLink::Response(response) => Ok(response),
            ResponseOrLink::Link(_) => bail!("Se esperaba contenido de Moodle"),
        }
    }

    pub async fn courses(&self) -> Result<(Vec<Course>, Option<String>)> {
        let mut courses = BTreeMap::new();
        let mut pages = Vec::new();
        let mut dashboard_error = None;
        for path in ["my/", "my/courses.php"] {
            match self.html(self.endpoint(path)).await {
                Ok(page) => pages.push(page),
                Err(error) if error.is::<SessionExpired>() => return Err(error),
                Err(error) => dashboard_error = Some(error),
            }
        }
        if pages.is_empty() {
            return Err(dashboard_error.context("No se pudo abrir el área personal de Moodle")?);
        }
        let key = pages
            .iter()
            .find_map(|page| SESSION_KEY.captures(&page.source).map(|m| m[1].to_owned()));
        for page in &pages {
            let doc = Document::from(page.source.as_str());
            for node in doc.select("a[href]").nodes() {
                let Some(url) = resolve(&page.url, &node.attr_or("href", "")) else {
                    continue;
                };
                if !self.owns(&url) || !url.path().ends_with("/course/view.php") {
                    continue;
                }
                let Some(id) = query_number(&url, "id") else {
                    continue;
                };
                let name = node_title(node);
                if id > 1 && !name.is_empty() {
                    courses.entry(id).or_insert(Course { id, name });
                }
            }
        }
        let warning = match key {
            Some(key) => match self.ajax_courses(&key, &mut courses).await {
                Ok(()) => None,
                Err(error) if error.is::<SessionExpired>() => return Err(error),
                Err(error) if courses.is_empty() => return Err(error),
                Err(_) => Some(
                    "Moodle no devolvió el catálogo completo. Se muestran los cursos encontrados."
                        .into(),
                ),
            },
            None if courses.is_empty() => bail!(
                "Moodle no publicó un catálogo de cursos en el área personal. Verifica la dirección y la sesión."
            ),
            None => {
                Some("Se muestran los cursos que aparecen como enlaces en el área personal.".into())
            }
        };
        let mut courses: Vec<_> = courses.into_values().collect();
        courses.sort_by_cached_key(|course| course.name.to_lowercase());
        Ok((courses, warning))
    }

    async fn ajax_courses(&self, key: &str, courses: &mut BTreeMap<u64, Course>) -> Result<()> {
        let mut endpoint = self.endpoint("lib/ajax/service.php");
        endpoint
            .query_pairs_mut()
            .append_pair("sesskey", key)
            .append_pair("info", COURSE_METHOD);
        let mut offset = 0_i64;
        for _ in 0..1000 {
            let request = self.post(endpoint.clone()).json(&json!([{
                "index": 0, "methodname": COURSE_METHOD,
                "args": {"offset": offset, "limit": PAGE_LIMIT, "classification": "all", "sort": "fullname"}
            }]));
            let response = self.response(request, Redirect::SameSite).await?;
            let html = read_html(response).await?;
            let body: Vec<Value> = serde_json::from_str(&html.source)
                .context("El catálogo de Moodle no devolvió una respuesta válida")?;
            let entry = body
                .first()
                .context("Moodle devolvió un catálogo sin datos")?;
            if entry["error"].as_bool() == Some(true) {
                if matches!(
                    entry["exception"]["errorcode"].as_str(),
                    Some("invalidsesskey" | "requireloginerror" | "servicerequireslogin")
                ) {
                    return Err(SessionExpired.into());
                }
                bail!("Moodle rechazó la consulta del catálogo");
            }
            let reply: CourseReply = serde_json::from_value(entry["data"].clone())
                .context("Moodle devolvió un catálogo en un formato no compatible")?;
            let size = reply.courses.len();
            for entry in reply.courses {
                let name = entry.fullname.or(entry.shortname).unwrap_or_default();
                let name = clean_text(&Document::fragment(name).text());
                courses.insert(
                    entry.id,
                    Course {
                        id: entry.id,
                        name: if name.is_empty() {
                            format!("Curso {}", entry.id)
                        } else {
                            name
                        },
                    },
                );
            }
            let next = match reply.nextoffset {
                Some(next) if next > offset => next,
                Some(_) => return Ok(()),
                None if size >= PAGE_LIMIT => offset + size as i64,
                None => return Ok(()),
            };
            if size == 0 {
                return Ok(());
            }
            offset = next;
        }
        bail!("El catálogo excedió el límite de paginación")
    }

    pub async fn course(&self, course: Course) -> Result<CourseDetail> {
        let mut url = self.endpoint("course/view.php");
        url.query_pairs_mut()
            .append_pair("id", &course.id.to_string());
        let page = self.html(url).await?;
        let mut loaded = HashSet::from([page.url.clone()]);
        let (mut sections, mut pending) = parse_course(self, &page, course.id)?;
        let mut warnings = Vec::new();
        while let Some((url, expected)) = pending.pop_front() {
            if !loaded.insert(url.clone()) {
                continue;
            }
            if loaded.len() > 400 {
                warnings.push("El curso tiene más páginas de sección de las que se pueden explorar en una consulta.".into());
                break;
            }
            let parsed = match self.html(url).await {
                Ok(page) => parse_course(self, &page, course.id),
                Err(error) => Err(error),
            };
            match parsed {
                Ok((mut additional, links)) => {
                    if additional.len() == 1 && additional[0].id == "general" {
                        let source = &mut additional[0];
                        source.id.clone_from(&expected.id);
                        source.number = expected.number;
                        source.name.clone_from(&expected.name);
                    }
                    for section in additional {
                        if let Some(existing) = sections.iter_mut().find(|s| {
                            s.id == section.id || (section.number > 0 && s.number == section.number)
                        }) {
                            for item in section.items {
                                if !existing.items.iter().any(|old| {
                                    old.id == item.id
                                        || (old.kind != "contenido" && old.url == item.url)
                                }) {
                                    existing.items.push(item);
                                }
                            }
                        } else if !section.items.is_empty() {
                            sections.push(section);
                        }
                    }
                    pending.extend(links);
                }
                Err(error) if error.is::<SessionExpired>() => return Err(error),
                Err(error) => warnings.push(format!("{} · {error}", expected.name)),
            }
        }
        sections.sort_by_key(|section| section.number);
        Ok(CourseDetail {
            course,
            sections,
            warnings,
        })
    }
}

fn is_secure(url: &Url) -> bool {
    url.scheme() == "https"
}

pub fn web_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let url = base.join(href).ok()?;
    web_url(&url).then_some(url)
}

pub fn login_path(url: &Url) -> bool {
    url.path().split('/').any(|part| part == "login")
}

pub async fn read_html(mut response: Response) -> Result<Html> {
    let url = response.url().clone();
    let encoding = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| {
            value
                .split(';')
                .skip(1)
                .filter_map(|pair| pair.trim().split_once('='))
                .find(|(key, _)| key.eq_ignore_ascii_case("charset"))
                .and_then(|(_, label)| {
                    encoding_rs::Encoding::for_label(label.trim_matches([' ', '"']).as_bytes())
                })
        })
        .unwrap_or(encoding_rs::UTF_8);
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.without_url())? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_HTML,
            "La página de Moodle es demasiado grande"
        );
        bytes.extend_from_slice(&chunk);
    }
    let (contents, _, _) = encoding.decode(&bytes);
    let source = contents.into_owned();
    if source.trim_start().starts_with('<') {
        let doc = Document::from(source.as_str());
        if !doc.select("body#page-login-index, form#login input[type=password], form[action*='/login/index.php'] input[type=password]").is_empty() {
            return Err(SessionExpired.into());
        }
        if !doc.select("#page-enrol-index, #region-main .errorcode, #region-main .alert-danger[role=alert]").is_empty() {
            bail!("Moodle indicó que el contenido no está disponible para esta sesión");
        }
    }
    Ok(Html { url, source })
}

pub fn query_number(url: &Url, key: &str) -> Option<u64> {
    url.query_pairs()
        .find(|(name, _)| name == key)?
        .1
        .parse()
        .ok()
}

pub fn decoded_filename(url: &Url) -> Option<String> {
    let part = url.path_segments()?.next_back()?;
    if part.is_empty() || part.ends_with(".php") {
        return None;
    }
    Some(percent_decode_str(part).decode_utf8_lossy().into_owned())
}

fn node_title(node: &NodeRef<'_>) -> String {
    let fragment = Document::fragment(node.inner_html());
    fragment
        .select(".accesshide, .sr-only, .visually-hidden, .activityiconcontainer")
        .remove();
    clean_text(&fragment.text())
}

fn section_id(node: &NodeRef<'_>, number: u64) -> String {
    node.attr("data-sectionid")
        .and_then(|id| id.parse::<u64>().ok())
        .map_or_else(|| format!("s{number}"), |id| id.to_string())
}

type SectionPages = std::collections::VecDeque<(Url, Section)>;

fn parse_course(
    session: &Session,
    page: &Html,
    course_id: u64,
) -> Result<(Vec<Section>, SectionPages)> {
    let doc = Document::from(page.source.as_str());
    let mut sections = Vec::new();
    let mut pending = SectionPages::new();
    let section_nodes = doc.select("li[id^='section-'], [data-for='section'][data-id], .course-section[data-sectionid], .course-content .section.main");
    let mut ids = HashSet::new();
    for node in section_nodes.nodes() {
        let number = node
            .attr("data-number")
            .and_then(|n| n.parse().ok())
            .or_else(|| node.attr("id")?.strip_prefix("section-")?.parse().ok())
            .or_else(|| node.attr("data-section")?.parse().ok())
            .unwrap_or(sections.len() as u64);
        let id = section_id(node, number);
        if !ids.insert(id.clone()) {
            continue;
        }
        let fragment = Document::fragment(node.inner_html());
        let heading = fragment.select_single(
            "h2.sectionname, h3.sectionname, h4.sectionname, .section-title, .sectionname",
        );
        let name = heading
            .nodes()
            .first()
            .map(node_title)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if number == 0 {
                    "General".into()
                } else {
                    format!("Sección {number}")
                }
            });
        let mut items = activity_items(session, &fragment, &page.url);
        if let Some(intro) = fragment
            .select_single(".section-summary, [data-for='sectioninfo'] .summary, .summary")
            .nodes()
            .first()
            && (!node_title(intro).is_empty()
                || !Document::fragment(intro.inner_html())
                    .select("img, a[href], iframe, video, audio")
                    .is_empty())
        {
            items.insert(
                0,
                inline_item(
                    format!("intro-{id}"),
                    "Contenido de la sección".into(),
                    intro.inner_html().to_string(),
                    &page.url,
                ),
            );
        }
        let section = Section {
            number,
            id,
            name,
            items,
        };
        for anchor in heading.select("a[href]").nodes() {
            let Some(url) = resolve(&page.url, &anchor.attr_or("href", "")) else {
                continue;
            };
            if session.owns(&url)
                && (url.path().ends_with("/course/section.php")
                    || (url.path().ends_with("/course/view.php")
                        && query_number(&url, "id") == Some(course_id)
                        && query_number(&url, "section").is_some()))
            {
                pending.push_back((url, section.clone()));
            }
        }
        sections.push(section);
    }
    if sections.is_empty() {
        let items = activity_items(session, &doc, &page.url);
        if !items.is_empty() {
            sections.push(Section {
                number: 0,
                id: "general".into(),
                name: "General".into(),
                items,
            });
        }
    }
    // Moodle themes may put links to individual sections outside the heading.
    if sections.iter().all(|section| section.items.is_empty()) && pending.is_empty() {
        for anchor in doc
            .select(".course-content a[href], #region-main a[href]")
            .nodes()
        {
            let Some(url) = resolve(&page.url, &anchor.attr_or("href", "")) else {
                continue;
            };
            let legacy = url.path().ends_with("/course/view.php")
                && query_number(&url, "id") == Some(course_id);
            if !session.owns(&url) || (!url.path().ends_with("/course/section.php") && !legacy) {
                continue;
            }
            let Some(number) = query_number(&url, if legacy { "section" } else { "id" }) else {
                continue;
            };
            let id = if legacy {
                format!("s{number}")
            } else {
                number.to_string()
            };
            let name = node_title(anchor);
            if !name.is_empty() {
                pending.push_back((
                    url,
                    Section {
                        number,
                        id,
                        name,
                        items: Vec::new(),
                    },
                ));
            }
        }
    }
    Ok((sections, pending))
}

fn inline_item(id: String, name: String, content: String, base: &Url) -> Item {
    Item {
        id,
        name,
        kind: "contenido".into(),
        url: base.clone(),
        content: Some(content),
    }
}

fn activity_items(session: &Session, doc: &Document, base: &Url) -> Vec<Item> {
    let mut items = Vec::new();
    let mut ids = HashSet::new();
    for node in doc.select(".activity[id^='module-'], [data-for='cmitem'], li.activity[class*='modtype_'], .activity-item[data-activityname]").nodes() {
        let classes = node.attr_or("class", "");
        let fragment = Document::fragment(node.inner_html());
        let kind = classes.split_whitespace().find_map(|class| class.strip_prefix("modtype_"))
            .or_else(|| node.has_class("label").then_some("label"));
        let node_id = node.attr("id")
            .and_then(|id| id.strip_prefix("module-")?.parse::<u64>().ok())
            .or_else(|| node.attr("data-id")?.parse::<u64>().ok());
        if matches!(kind, Some("label")) {
            let body = fragment.select_single(".activity-description, .contentwithoutlink, .no-overflow");
            let content = if body.is_empty() { fragment.inner_html() } else { body.inner_html() };
            let id = node_id.map(|n| format!("m{n}")).unwrap_or_else(|| short_hash(content.as_bytes()));
            if ids.insert(id.clone()) {
                let name: String = clean_text(&body.text()).chars().take(85).collect();
                items.push(inline_item(id, if name.is_empty() { "Texto y multimedia".into() } else { name }, content.to_string(), base));
            }
            continue;
        }
        for anchor in fragment.select("a[href]").nodes() {
            let Some(mut url) = resolve(base, &anchor.attr_or("href", "")) else { continue };
            if !session.owns(&url) {
                continue;
            }
            let type_from_url = url.path().rsplit_once("/mod/")
                .and_then(|(_, tail)| tail.strip_suffix("/view.php")?.split('/').next());
            let is_file = url.path().contains("/pluginfile.php/");
            let Some(kind) = type_from_url.or_else(|| is_file.then_some("resource")) else { continue };
            let kind = kind.to_owned();
            let from_name = fragment.select_single(".activityname .instancename, .instancename");
            let title = from_name.nodes().first().map(node_title)
                .filter(|name| !name.is_empty()).unwrap_or_else(|| node_title(anchor));
            let name = if title.is_empty() { decoded_filename(&url).unwrap_or_else(|| "Actividad".into()) } else { title };
            url.set_fragment(None);
            let id = node_id.or_else(|| query_number(&url, "id"))
                .map(|id| format!("m{id}")).unwrap_or_else(|| short_hash(url.as_str().as_bytes()));
            if ids.insert(id.clone()) {
                items.push(Item { id, name, kind, url, content: None });
            }
            break;
        }
    }
    items
}
