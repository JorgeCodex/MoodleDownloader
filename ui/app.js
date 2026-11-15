"use strict";

const $ = (id) => document.getElementById(id);
const state = {
  connected: false,
  editingSession: true,
  mode: null,
  cancelling: false,
  view: "library",
  courses: [],
  detail: null,
  activeId: null,
  detailError: "",
  selection: new Set(),
  counts: { saved: 0, existing: 0, error: 0 },
  noticeTimer: null
};
const kinds = {
  resource: "Archivo", folder: "Carpeta", assign: "Tarea", quiz: "Cuestionario",
  page: "Página", book: "Libro", url: "Enlace", contenido: "Contenido", forum: "Foro"
};
const levels = {
  batch: "Curso", course: "Curso", item: "Actividad", saved: "Guardado",
  existing: "Ya guardado", error: "Incidencia", info: "Información"
};

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function icon(name) {
  const node = el("span", "icon icon-" + name);
  node.setAttribute("aria-hidden", "true");
  return node;
}

function button(label, style, handler, symbol, idleOnly = false) {
  const node = el("button", style);
  node.type = "button";
  if (symbol) node.append(icon(symbol));
  node.append(document.createTextNode(label));
  node.addEventListener("click", handler);
  if (idleOnly) {
    node.dataset.idleOnly = "";
    node.disabled = Boolean(state.mode);
  }
  return node;
}

function send(action, fields = {}) {
  try {
    window.ipc.postMessage(JSON.stringify({ action, ...fields }));
  } catch {
    notice("La interfaz de escritorio no está disponible.");
  }
}

function remember(key, value) {
  try { localStorage.setItem(key, value); } catch { /* Preferences are optional. */ }
}

function remembered(key) {
  try { return localStorage.getItem(key) || ""; } catch { return ""; }
}

function notice(text) {
  $("notice").textContent = text;
  $("notice").hidden = false;
  clearTimeout(state.noticeTimer);
  state.noticeTimer = setTimeout(() => { $("notice").hidden = true; }, 6200);
}

function connectionError(text) {
  $("connection-error").textContent = text;
  $("connection-error").hidden = !text;
}

function selectView(view) {
  state.view = view;
  $("library").hidden = view !== "library";
  $("activity").hidden = view !== "activity";
  $("page-label").textContent = view === "library" ? "Cursos" : "Actividad";
  document.querySelectorAll(".nav-button").forEach((element) => {
    const active = element.dataset.view === view;
    element.classList.toggle("current", active);
    if (active) element.setAttribute("aria-current", "page");
    else element.removeAttribute("aria-current");
  });
  updateChrome();
  window.scrollTo(0, 0);
}

function updateChrome() {
  const viewCatalog = state.connected && !state.editingSession;
  $("connection").hidden = viewCatalog;
  $("catalog").hidden = !viewCatalog;
  $("change-session").hidden = !viewCatalog || state.view !== "library";
  $("return-library").hidden = !state.connected || !state.editingSession;
  $("disconnect").hidden = !state.connected;
  $("session-dot").classList.toggle("online", state.connected);
  $("session-label").textContent = state.connected ? "Sesión conectada" : "Sin conexión";
  if (!state.connected) $("session-host").textContent = "Tu aula virtual";
  const title = $("hero-title");
  if (viewCatalog) {
    title.textContent = "Tu biblioteca de Moodle";
    $("hero-subtitle").textContent = "Explora tus cursos y elige qué quieres guardar.";
  } else {
    title.replaceChildren("Tu aula virtual,", document.createElement("br"), "también sin conexión.");
    $("hero-subtitle").textContent = "Guarda los materiales de Moodle en tu PC.";
  }
  toggleControls();
}

function toggleControls() {
  const busy = Boolean(state.mode);
  for (const id of ["address", "cookie", "toggle-cookie", "change-session", "disconnect", "return-library", "choose-folder", "target"]) {
    $(id).disabled = busy;
  }
  document.querySelectorAll("[data-idle-only]").forEach((node) => { node.disabled = busy; });
  $("connect").disabled = busy;
  $("cancel-connect").hidden = state.mode !== "connect";
  $("cancel-connect").disabled = state.cancelling;
  $("cancel-job").disabled = state.cancelling;
  updateSelection();
}

function normalizeSearch(text) {
  return text.normalize("NFD").replace(/\p{Diacritic}/gu, "").toLocaleLowerCase("es").trim();
}

function visibleCourses() {
  const query = normalizeSearch($("search").value);
  return state.courses.filter((course) => normalizeSearch(course.name).includes(query));
}

function renderCourses() {
  const visible = visibleCourses();
  const list = $("course-list");
  const scroll = list.scrollTop;
  const fragment = document.createDocumentFragment();
  for (const course of visible) {
    const row = el("div", "course-row");
    row.classList.toggle("active", state.activeId === course.id);
    const check = document.createElement("input");
    check.type = "checkbox";
    check.checked = state.selection.has(course.id);
    check.dataset.courseCheck = String(course.id);
    check.setAttribute("aria-label", "Seleccionar " + course.name);
    check.addEventListener("change", () => {
      if (check.checked) state.selection.add(course.id);
      else state.selection.delete(course.id);
      updateSelection();
    });
    const name = button(course.name, "course-name", () => chooseCourse(course.id), null, true);
    name.dataset.courseId = String(course.id);
    if (state.activeId === course.id) name.setAttribute("aria-current", "true");
    row.append(check, name);
    fragment.append(row);
  }
  if (!visible.length) {
    fragment.append(el("p", "empty", state.courses.length ? "No hay cursos que coincidan." : "Moodle no devolvió cursos inscritos."));
  }
  list.replaceChildren(fragment);
  list.scrollTop = scroll;
  $("course-count").textContent = state.courses.length;
  updateSelection(visible);
}

function updateSelection(visible = visibleCourses()) {
  const count = visible.filter((course) => state.selection.has(course.id)).length;
  const select = $("select-visible");
  select.checked = visible.length > 0 && count === visible.length;
  select.indeterminate = count > 0 && count < visible.length;
  select.disabled = visible.length === 0;
  $("selection-count").textContent = state.selection.size ? String(state.selection.size) : "";
  $("selection-count").title = state.selection.size + " cursos seleccionados";
  $("download-selection").disabled = Boolean(state.mode) || state.selection.size === 0;
}

function chooseCourse(id) {
  if (!state.connected || state.mode) return;
  if (state.activeId === id && state.detail) return;
  state.activeId = id;
  state.detail = null;
  state.detailError = "";
  renderCourses();
  renderDetail(true);
  send("course", { id });
}

function renderDetail(loading = false) {
  const panel = $("detail");
  panel.replaceChildren();
  if (!state.detail) {
    const note = loading
      ? "Consultando las secciones del curso…"
      : state.detailError || "Elige un curso para explorar su contenido.";
    const empty = el("div", "empty");
    empty.append(el("p", null, note));
    if (loading) {
      empty.append(button("Cancelar consulta", "quiet cancel-course", () => send("cancel")));
    }
    panel.append(empty);
    return;
  }
  const { course, sections, warnings } = state.detail;
  const amount = sections.reduce((total, section) => total + section.items.length, 0);
  const heading = el("div", "detail-heading");
  const title = el("div");
  title.append(el("h2", null, course.name), el("p", null, amount + (amount === 1 ? " actividad" : " actividades")));
  const download = button("Descargar curso", "secondary", () => downloadItems([course.id]), "download", true);
  heading.append(title, download);
  panel.append(heading);
  for (const warning of warnings) panel.append(el("p", "inline-warning", warning));
  if (!amount) {
    panel.append(el("p", "empty", "Este curso no muestra actividades accesibles."));
    return;
  }
  let first = true;
  for (const section of sections) {
    if (!section.items.length) continue;
    const block = el("details", "section");
    block.open = first;
    first = false;
    const summary = el("summary", null, section.name);
    summary.append(el("span", "count", section.items.length));
    block.append(summary);
    for (const item of section.items) {
      const row = el("div", "item-row");
      const label = el("div", "item-label");
      const info = el("div", "item-info", item.name);
      info.append(el("span", "item-type", kinds[item.kind] || "Acceso en Moodle"));
      label.append(icon(item.kind === "folder" ? "folder" : "file"), info);
      const actions = el("div", "item-actions");
      const open = button("Ver", "quiet", () => send("open_url", { url: item.url }));
      open.title = "Abrir en Moodle";
      const save = button("Guardar", "quiet", () => downloadItems([course.id], section.id + "/" + item.id), "download", true);
      save.title = "Descargar solo esta actividad";
      actions.append(open, save);
      row.append(label, actions);
      block.append(row);
    }
    panel.append(block);
  }
}

function downloadItems(ids, item = null) {
  if (state.mode || !state.connected) return;
  const target = $("target").value.trim();
  if (!target) {
    notice("Elige una carpeta de descargas para continuar.");
    $("target").focus();
    return;
  }
  remember("target", target);
  send("download", { ids, item, target });
}

function setJobBar(title, text, status) {
  $("job-bar").hidden = false;
  $("job-title").textContent = title;
  $("job-detail").textContent = text;
  $("job-indicator").classList.toggle("moving", status === "running");
  $("job-indicator").classList.toggle("warn", status === "warning");
  $("cancel-job").hidden = status !== "running";
  $("dismiss-job").hidden = status === "running";
}

function updateMetrics() {
  $("metric-saved").textContent = state.counts.saved;
  $("metric-existing").textContent = state.counts.existing;
  $("metric-errors").textContent = state.counts.error;
}

function log(level, text) {
  const list = $("activity-log");
  const now = new Date();
  if (list.firstElementChild?.classList.contains("empty")) list.replaceChildren();
  const line = el("div", "log-entry " + level);
  const time = el("time", null, now.toLocaleTimeString("es", { hour12: false }));
  time.dateTime = now.toISOString();
  line.append(time, el("strong", null, levels[level] || "Información"), el("span", null, text));
  list.prepend(line);
  if (list.children.length > 1500) list.lastElementChild.remove();
  if (Object.hasOwn(state.counts, level)) {
    state.counts[level] += 1;
    updateMetrics();
  }
}

function resetLog() {
  $("activity-log").replaceChildren(el("p", "empty", "Aquí aparecerán los archivos guardados y las incidencias."));
  state.counts = { saved: 0, existing: 0, error: 0 };
  updateMetrics();
}

function hideCookie() {
  $("cookie").type = "password";
  $("toggle-cookie").textContent = "Mostrar";
  $("toggle-cookie").setAttribute("aria-pressed", "false");
}

function clearConnection() {
  state.connected = false;
  state.editingSession = true;
  state.courses = [];
  state.selection.clear();
  state.detail = null;
  state.activeId = null;
  state.detailError = "";
  $("search").value = "";
  $("cookie").value = "";
  hideCookie();
  renderCourses();
  renderDetail();
  updateChrome();
}

window.receiveMoodle = (message) => {
  switch (message.type) {
    case "started":
      state.mode = message.mode;
      state.cancelling = false;
      if (message.mode === "connect") {
        $("connect").textContent = "Conectando…";
      } else if (message.mode === "download") {
        setJobBar("Preparando descarga", "Consultando el contenido de Moodle", "running");
      }
      toggleControls();
      break;
    case "cancelling":
      state.cancelling = true;
      if (state.mode === "download") $("job-detail").textContent = "Cancelando la transferencia…";
      toggleControls();
      break;
    case "catalog":
      finishJob();
      state.connected = true;
      state.editingSession = false;
      state.courses = message.courses;
      state.detail = null;
      state.activeId = null;
      state.detailError = "";
      state.selection.clear();
      $("search").value = "";
      remember("address", $("address").value.trim());
      $("session-host").textContent = $("address").value.trim().replace(/^https?:\/\//i, "").replace(/\/$/, "");
      $("catalog-warning").textContent = message.warning || "";
      $("catalog-warning").hidden = !message.warning;
      renderCourses();
      renderDetail();
      updateChrome();
      if (message.courses.length) chooseCourse(message.courses[0].id);
      break;
    case "detail":
      finishJob();
      state.detail = message.detail;
      state.activeId = message.detail.course.id;
      renderDetail();
      break;
    case "target":
      $("target").value = message.target;
      remember("target", message.target);
      break;
    case "disconnected":
      clearConnection();
      connectionError("");
      selectView("library");
      break;
    case "progress": {
      const { level, text } = message.update;
      if (level === "batch") $("job-title").textContent = text;
      else if (level !== "course") $("job-detail").textContent = text;
      if (level !== "transfer" && level !== "course") log(level, text);
      break;
    }
    case "report": {
      finishJob();
      const { saved, reused, errors, steps } = message.report;
      const summary = saved + " guardados · " + reused + " ya guardados · " + errors + " incidencias";
      const title = errors ? "Descarga con incidencias" : steps ? "Descarga completada" : "Sin actividades para descargar";
      log("info", summary);
      setJobBar(title, summary, errors ? "warning" : "done");
      break;
    }
    case "failed":
      finishJob();
      if (message.expired) {
        clearConnection();
        selectView("library");
      }
      if (message.mode === "connect" || message.expired) connectionError(message.text);
      if (message.mode === "course" && !message.expired) {
        state.detailError = message.text;
        renderDetail();
      }
      if (message.mode === "download") {
        log("error", message.text);
        setJobBar("Descarga interrumpida", message.text, "warning");
      }
      break;
    case "cancelled":
      finishJob();
      if (message.mode === "course") renderDetail();
      if (message.mode === "download") {
        log("info", "La descarga fue cancelada.");
        setJobBar("Descarga cancelada", "Los archivos completados siguen en su carpeta.", "warning");
      }
      break;
    case "notice":
      notice(message.text);
      break;
  }
};

function finishJob() {
  state.mode = null;
  state.cancelling = false;
  $("connect").replaceChildren(icon("link"), "Conectar y ver cursos");
  toggleControls();
}

$("address").value = remembered("address") || $("address").value;
$("target").value = remembered("target");
$("search").addEventListener("input", renderCourses);
$("select-visible").addEventListener("change", (event) => {
  const checked = event.target.checked;
  for (const course of visibleCourses()) {
    if (checked) state.selection.add(course.id);
    else state.selection.delete(course.id);
  }
  $("course-list").querySelectorAll("[data-course-check]").forEach((node) => {
    node.checked = state.selection.has(Number(node.dataset.courseCheck));
  });
  updateSelection();
});
document.querySelectorAll(".nav-button").forEach((node) => {
  node.addEventListener("click", () => selectView(node.dataset.view));
});
$("connect-form").addEventListener("submit", (event) => {
  event.preventDefault();
  if (state.mode) return;
  const address = $("address").value.trim();
  const cookie = $("cookie").value.trim();
  if (!address || !cookie) return connectionError("Completa la dirección y el valor de MoodleSession.");
  connectionError("");
  $("cookie").value = "";
  hideCookie();
  send("connect", { address, cookie });
});
$("toggle-cookie").addEventListener("click", () => {
  if ($("cookie").type === "text") {
    hideCookie();
  } else {
    $("cookie").type = "text";
    $("toggle-cookie").textContent = "Ocultar";
    $("toggle-cookie").setAttribute("aria-pressed", "true");
  }
});
$("change-session").addEventListener("click", () => {
  state.editingSession = true;
  connectionError("");
  updateChrome();
  $("cookie").focus();
});
$("return-library").addEventListener("click", () => {
  state.editingSession = false;
  updateChrome();
});
$("disconnect").addEventListener("click", () => send("disconnect"));
$("choose-folder").addEventListener("click", () => send("choose_folder", { target: $("target").value }));
$("target").addEventListener("change", () => remember("target", $("target").value.trim()));
$("open-folder").addEventListener("click", () => send("open_folder", { target: $("target").value }));
$("activity-open-folder").addEventListener("click", () => send("open_folder", { target: $("target").value }));
$("download-selection").addEventListener("click", () => downloadItems([...state.selection]));
$("cancel-connect").addEventListener("click", () => send("cancel"));
$("cancel-job").addEventListener("click", () => send("cancel"));
$("job-activity").addEventListener("click", () => selectView("activity"));
$("dismiss-job").addEventListener("click", () => { $("job-bar").hidden = true; });
$("clear-log").addEventListener("click", resetLog);
renderCourses();
renderDetail();
resetLog();
updateChrome();
