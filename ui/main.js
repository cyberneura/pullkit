const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const reposEl = document.querySelector("#repos");
const syncButton = document.querySelector("#sync");
const refreshButton = document.querySelector("#refresh");
const selectAll = document.querySelector("#select-all");
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

function selectedNames() {
  return [...document.querySelectorAll(".repo-check:checked")].map((input) => input.value);
}

function updateSelection() {
  const all = [...document.querySelectorAll(".repo-check:not(:disabled)")];
  const selected = selectedNames().length;
  countEl.textContent = `${selected} selected`;
  syncButton.disabled = syncing || selected === 0;
  refreshButton.disabled = syncing || runningLoads.size > 0;
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

function commitCells(commits) {
  if (commits === "pending") return { local: "-", remote: "-", difference: "fetching…", kind: "pending", error: "" };
  if (commits === null) return { local: "-", remote: "-", difference: "-", kind: "pending", error: "" };
  const date = (commit) => (commit ? commit.date : "-");
  let kind = "error";
  if (commits.difference === "up to date") kind = "same";
  else if (commits.difference?.endsWith("behind")) kind = "behind";
  else if (commits.difference?.endsWith("ahead")) kind = "ahead";
  else if (commits.difference) kind = "diverged";
  return {
    local: date(commits.local),
    remote: date(commits.remote),
    difference: commits.difference || (commits.error ? "unavailable" : "-"),
    kind,
    error: commits.error || "",
  };
}

function commitsHtml(commits) {
  const cells = commitCells(commits);
  return `<span class="commit"><small>local</small>${escapeHtml(cells.local)}</span>
    <span class="commit"><small>remote</small>${escapeHtml(cells.remote)}</span>
    <span class="difference ${cells.kind}" title="${escapeHtml(cells.error)}">${escapeHtml(cells.difference)}</span>`;
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
    const repos = await invoke("list_repos");
    if (token !== inspectionToken) return;
    if (!repos.length) {
      reposEl.innerHTML = '<p class="empty muted">No repositories in config.yaml.</p>';
    } else {
      reposEl.innerHTML = repos.map((repo) => {
        const [kind, text] = statusFor(repo);
        const missing = !repo.path_exists;
        return `<label class="repo${missing ? " missing" : ""}" title="${escapeHtml(repo.error || "")}">
          <input class="repo-check" data-annotate="checkbox-repository" type="checkbox" value="${escapeHtml(repo.name)}"${missing ? " disabled" : ""} />
          <span class="repo-info"><span class="repo-name">${escapeHtml(repo.name)}</span><span class="repo-path">${escapeHtml(repo.path)}</span></span>
          <span class="status ${kind}">${escapeHtml(text)}</span>
          <span class="commits" data-annotate="repository-commits" data-repo="${escapeHtml(repo.name)}" data-path="${escapeHtml(repo.path)}">${commitsHtml(missing ? null : "pending")}</span>
        </label>`;
      }).join("");
      inspecting = true;
    }
    document.querySelectorAll(".repo-check").forEach((el) => el.addEventListener("change", updateSelection));
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
  document.querySelectorAll(".repo-check:not(:disabled)").forEach((el) => { el.checked = selectAll.checked; });
  updateSelection();
});
refreshButton.addEventListener("click", refreshRepos);

syncButton.addEventListener("click", async () => {
  const names = selectedNames();
  // An empty list means "every repository" to the backend, which is never what
  // an empty selection should do here.
  if (!names.length) return;
  syncing = true;
  updateSelection();
  panesEl.innerHTML = "";
  logEl.textContent = `pullkit run: ${names.length} repositories\n`;
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
  const el = document.querySelector(`.commits[data-repo="${CSS.escape(commits.name)}"]`);
  // The list and the inspection read the config separately, so an edit between
  // the two can point one name at a different directory. Only the row that was
  // drawn for this directory may take the result.
  if (el && el.dataset.path === commits.path) el.innerHTML = commitsHtml(commits);
}

// The first load has to wait for the listeners: a fetch that finishes before
// `listen` has registered would leave its row on the placeholder.
Promise.all([listen("commit-inspected", showCommits), listen("sync-event", showSyncEvent)]).then(refreshRepos);
