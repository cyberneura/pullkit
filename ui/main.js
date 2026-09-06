const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const reposEl = document.querySelector("#repos");
const syncButton = document.querySelector("#sync");
const refreshButton = document.querySelector("#refresh");
const selectAll = document.querySelector("#select-all");
const selectOutdatedButton = document.querySelector("#select-outdated");
const countEl = document.querySelector("#selection-count");
const logEl = document.querySelector("#log");
const panesEl = document.querySelector("#panes");
const runState = document.querySelector("#run-state");
let syncing = false;
// Seeded per page rather than from zero, so a run left over from an earlier
// page cannot reuse a token this page is about to hand out.
let inspectionToken = Date.now();
// A load stays here from the moment it is started until its background
// `git fetch` calls have finished, so that a sync can say what it is waiting
// for rather than stopping on a lock with nothing in the log.
const runningLoads = new Set();
// The sync this page started, if any. A page reloaded during a sync goes on
// receiving that sync's events, which must not land in the panes of the one it
// starts next.
let syncToken = null;
// Read with the repository list, so that an edit to the configuration takes
// effect on the next refresh rather than only on a restart.
let selectOutdatedByDefault = false;

function selectedNames() {
  return [...document.querySelectorAll(".repo-check:checked")].map((input) => input.value);
}

function outdatedChecks() {
  return [...document.querySelectorAll('.repo[data-relation="behind"] .repo-check:not(:disabled)')];
}

function updateSelection() {
  const all = [...document.querySelectorAll(".repo-check:not(:disabled)")];
  const selected = selectedNames().length;
  countEl.textContent = `${selected} selected`;
  syncButton.disabled = syncing || selected === 0;
  refreshButton.disabled = syncing || runningLoads.size > 0;
  // Off while the fetches are still running, because until they are in there
  // is no telling which rows are outdated, and off afterwards when none are,
  // which says so more plainly than a button that does nothing.
  selectOutdatedButton.disabled =
    syncing || runningLoads.size > 0 || outdatedChecks().length === 0;
  selectAll.checked = all.length > 0 && selected === all.length;
  selectAll.indeterminate = selected > 0 && selected < all.length;
}

function statusFor(repo) {
  if (!repo.path_exists) return ["missing", "Missing"];
  if (repo.error) return ["error", "Error"];
  if (!repo.clean) return ["dirty", "Dirty"];
  if (!repo.on_main) return ["branch", repo.branch || "Other branch"];
  return ["", "Ready"];
}

// The backend sends the ancestry as well as its wording. Deriving the kind
// from the wording, as this once did, ties the colour and "Select all
// outdated" to text that is written for people and may be reworded.
const RELATION_KINDS = { same: "same", behind: "behind", ahead: "ahead", diverged: "diverged" };

function commitCells(commits) {
  if (commits === "pending") return { local: "-", remote: "-", difference: "fetching…", kind: "pending", error: "" };
  if (commits === null) return { local: "-", remote: "-", difference: "-", kind: "pending", error: "" };
  const date = (commit) => (commit ? commit.date : "-");
  return {
    local: date(commits.local),
    remote: date(commits.remote),
    difference: commits.difference || (commits.error ? "unavailable" : "-"),
    kind: RELATION_KINDS[commits.relation] || "error",
    error: commits.error || "",
  };
}

function commitCellsHtml(cells) {
  return `<td class="col-date col-local">${escapeHtml(cells.local)}</td>
    <td class="col-date col-remote">${escapeHtml(cells.remote)}</td>
    <td class="col-difference"><span class="difference ${cells.kind}" title="${escapeHtml(cells.error)}">${escapeHtml(cells.difference)}</span></td>`;
}

// A table rather than a row of flex items: every cell of a column is then as
// wide as the column, so a wider status pill or a shorter date cannot push the
// cells after it out of line with the rows above. The row was a `<label>`
// before, which is what named each checkbox; a `<tr>` cannot wrap one, so the
// name is given with `aria-label` instead. Without it a screen reader, and the
// accessibility tree the GUI is driven through in tests, sees a column of
// checkboxes with nothing to tell them apart.
function repoTableHtml(repos) {
  const rows = repos.map((repo) => {
    const [kind, text] = statusFor(repo);
    const missing = !repo.path_exists;
    const cells = commitCells(missing ? null : "pending");
    return `<tr class="repo${missing ? " missing" : ""}" data-annotate="repository-row"
      data-repo="${escapeHtml(repo.name)}" data-path="${escapeHtml(repo.path)}" data-relation="${cells.kind}"
      title="${escapeHtml(repo.error || "")}">
      <td class="col-check"><input class="repo-check" data-annotate="checkbox-repository" type="checkbox" aria-label="${escapeHtml(repo.name)}" value="${escapeHtml(repo.name)}"${missing ? " disabled" : ""} /></td>
      <td class="col-repo"><span class="repo-name">${escapeHtml(repo.name)}</span><span class="repo-path">${escapeHtml(repo.path)}</span></td>
      <td class="col-status"><span class="status ${kind}">${escapeHtml(text)}</span></td>
      ${commitCellsHtml(cells)}
    </tr>`;
  }).join("");
  return `<table class="repo-table">
    <thead><tr>
      <th class="col-check"></th>
      <th class="col-repo">Repository</th>
      <th class="col-status">Status</th>
      <th class="col-date">Local</th>
      <th class="col-date">Remote</th>
      <th class="col-difference">Difference</th>
    </tr></thead>
    <tbody>${rows}</tbody>
  </table>`;
}

function refreshRepos() {
  const load = loadRepos().finally(() => { runningLoads.delete(load); updateSelection(); });
  runningLoads.add(load);
  updateSelection();
  return load;
}

async function loadRepos() {
  const token = ++inspectionToken;
  let inspecting = false;
  reposEl.innerHTML = '<p class="empty muted">Loading configuration…</p>';
  // The rows are gone, so nothing is selected and the sync button turns off
  // until the new list is drawn.
  updateSelection();
  try {
    // Both come from the same file, so one failure covers both.
    const outdatedByDefault = await invoke("select_outdated_by_default");
    const repos = await invoke("list_repos");
    if (token !== inspectionToken) return;
    // Adopted only once this load is known to be the current one, or a slow
    // reply from a superseded load would set the value the new list uses.
    selectOutdatedByDefault = outdatedByDefault;
    if (!repos.length) {
      reposEl.innerHTML = '<p class="empty muted">No repositories in config.yaml.</p>';
    } else {
      reposEl.innerHTML = repoTableHtml(repos);
      inspecting = true;
    }
  } catch (error) {
    reposEl.innerHTML = `<p class="empty status error">${escapeHtml(String(error))}</p>`;
  }
  updateSelection();
  // The list is on screen already; the remote fetches run on the worker pool in
  // the backend and each row is filled in as its own fetch completes.
  if (!inspecting) return;
  try {
    await invoke("inspect_all_commits", { token });
  } catch (error) {
    logEl.textContent += `\nERROR could not inspect repositories: ${error}`;
  }
}

function escapeHtml(value) {
  return value.replace(/[&<>"']/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char]);
}

selectAll.addEventListener("change", () => {
  document.querySelectorAll(".repo-check:not(:disabled)").forEach((el) => {
    el.checked = selectAll.checked;
    markDecided(el.closest(".repo"));
  });
  updateSelection();
});

// Sets the selection to exactly the outdated repositories rather than adding
// to it: with the rows already picked left alone there would be no way to ask
// for "the outdated ones" once anything else had been chosen, and "Select all"
// is there to clear.
function selectOutdated() {
  document.querySelectorAll(".repo-check:not(:disabled)").forEach((el) => {
    const row = el.closest(".repo");
    el.checked = row?.dataset.relation === "behind";
    markDecided(row);
  });
  updateSelection();
}

// A row the user has decided about is left alone by
// `select_outdated_by_default`, whether the decision came before or after its
// fetch. Without this a row turned off while its fetch was still running would
// come back on when the result landed.
function markDecided(row) {
  if (row) row.dataset.decided = "true";
}

selectOutdatedButton.addEventListener("click", selectOutdated);
refreshButton.addEventListener("click", refreshRepos);

// The rows are replaced on every load, so the list itself carries the
// listeners rather than each row getting its own.
reposEl.addEventListener("change", (event) => {
  if (!event.target.classList.contains("repo-check")) return;
  markDecided(event.target.closest(".repo"));
  updateSelection();
});

// A row was a label before it became a table row, and clicking anywhere on it
// went on toggling the box. A click on the box itself is left alone, or it
// would be toggled twice and stay as it was.
reposEl.addEventListener("click", (event) => {
  if (event.target.classList.contains("repo-check")) return;
  const row = event.target.closest(".repo");
  const check = row?.querySelector(".repo-check");
  if (!check || check.disabled) return;
  check.checked = !check.checked;
  markDecided(row);
  updateSelection();
});

syncButton.addEventListener("click", async () => {
  // An empty list means "every repository" to the backend, which is never what
  // an empty selection should do here.
  if (!selectedNames().length) return;
  syncing = true;
  updateSelection();
  panesEl.innerHTML = "";
  logEl.textContent = "";
  runState.textContent = "Running";
  runState.className = "badge running";
  try {
    // The backend takes a per-repository lock, so a sync would block rather than
    // race with a background fetch. Waiting here instead keeps the run log
    // honest about what it is waiting for.
    if (runningLoads.size) {
      logEl.textContent += "waiting for the remote inspection to finish\n";
      await Promise.all(runningLoads);
    }
    // Read after the wait rather than before it: rows go on being selected
    // while it runs, by `select_outdated_by_default` or by hand, and the run
    // has to be what the list showed when it started.
    const names = selectedNames();
    if (!names.length) {
      logEl.textContent += "nothing is selected any more\n";
      runState.textContent = "Ready";
      runState.className = "badge";
      return;
    }
    logEl.textContent += `pullkit run: ${names.length} repositories\n`;
    syncToken = Date.now();
    const results = await invoke("sync_selected", { names, token: syncToken });
    logEl.textContent += "\nSummary\n";
    results.forEach((result) => { logEl.textContent += `  ${result.name.padEnd(20)} ${result.outcome}: ${result.message}\n`; });
    runState.textContent = "Complete";
    runState.className = "badge done";
    refreshRepos();
  } catch (error) {
    logEl.textContent += `\nERROR ${error}`;
    runState.textContent = "Failed";
    runState.className = "badge";
  } finally {
    syncing = false;
    updateSelection();
  }
});

const FAILED_OUTCOMES = new Set(["pull_failed", "build_failed", "status_failed"]);
// Lines kept per pane. A build can write far more, and a page holding every
// line of several of them at once grows until it stops responding.
const PANE_HISTORY = 2000;

function paneHtml(index) {
  return `<div class="pane" data-annotate="sync-pane" data-worker="${index}">
    <div class="pane-title waiting"><span class="pane-repo">worker ${index + 1}</span><span class="pane-state">waiting</span></div>
    <pre class="pane-log"></pre>
  </div>`;
}

function pane(worker) {
  return panesEl.querySelector(`.pane[data-worker="${worker}"]`);
}

function setPaneState(worker, repo, state, kind) {
  const el = pane(worker);
  if (!el) return;
  const title = el.querySelector(".pane-title");
  title.className = `pane-title ${kind}`;
  title.querySelector(".pane-repo").textContent = `worker ${worker + 1} · ${repo}`;
  title.querySelector(".pane-state").textContent = state;
}

function lineClass(line) {
  if (line.startsWith("ERROR ")) return "error";
  if (line.startsWith("WARN ")) return "warn";
  if (line.startsWith("OK ")) return "ok";
  return "";
}

// One pane per worker, laid out when the backend says how many it uses. A
// pane keeps the lines of every repository its worker handled, so an error
// from an earlier one stays readable until the run is over.
function showSyncEvent(event) {
  const { token, event: payload } = event.payload;
  if (token !== syncToken) return;
  if (payload.kind === "planned") {
    panesEl.innerHTML = Array.from({ length: payload.workers }, (_, index) => paneHtml(index)).join("");
    logEl.textContent += `running ${payload.workers} at a time\n`;
    return;
  }
  if (payload.kind === "started") {
    // The pane keeps the log of the repositories before this one, so the new
    // one is set off from them.
    const log = pane(payload.worker)?.querySelector(".pane-log");
    if (log && log.childNodes.length) {
      const separator = document.createElement("span");
      separator.className = "separator";
      separator.textContent = `\n── ${payload.name}\n`;
      log.appendChild(separator);
    }
    setPaneState(payload.worker, payload.name, "running", "running");
    return;
  }
  if (payload.kind === "line") {
    const log = pane(payload.worker)?.querySelector(".pane-log");
    if (!log) return;
    const kind = lineClass(payload.line);
    const span = document.createElement("span");
    if (kind) span.className = kind;
    span.textContent = `${payload.line}\n`;
    log.appendChild(span);
    while (log.childNodes.length > PANE_HISTORY) log.removeChild(log.firstChild);
    log.scrollTop = log.scrollHeight;
    return;
  }
  if (payload.kind === "finished") {
    const { result } = payload;
    const kind = FAILED_OUTCOMES.has(result.outcome) ? "failed" : "done";
    setPaneState(payload.worker, result.name, result.outcome.replace(/_/g, " "), kind);
  }
}

function showCommits(event) {
  const { token, commits } = event.payload;
  if (token !== inspectionToken) return;
  const row = document.querySelector(`.repo[data-repo="${CSS.escape(commits.name)}"]`);
  // The list and the inspection read the config separately, so an edit between
  // the two can point one name at a different directory. Only the row that was
  // drawn for this directory may take the result.
  if (!row || row.dataset.path !== commits.path) return;
  const cells = commitCells(commits);
  row.dataset.relation = cells.kind;
  row.querySelector(".col-local").textContent = cells.local;
  row.querySelector(".col-remote").textContent = cells.remote;
  const difference = row.querySelector(".col-difference .difference");
  difference.className = `difference ${cells.kind}`;
  difference.title = cells.error;
  difference.textContent = cells.difference;
  // Each row is ticked as its own inspection arrives, and only then. A row the
  // user has already decided about keeps their answer.
  const check = row.querySelector(".repo-check");
  if (
    selectOutdatedByDefault
    && cells.kind === "behind"
    && !row.dataset.decided
    && check
    && !check.disabled
  ) {
    check.checked = true;
  }
  // The button turns on once a row is known to be outdated, so this runs for
  // every result and not only the ones that tick a box.
  updateSelection();
}

// The first load has to wait for the listeners: a fetch that finishes before
// `listen` has registered would leave its row on the placeholder.
Promise.all([listen("commit-inspected", showCommits), listen("sync-event", showSyncEvent)]).then(refreshRepos);
