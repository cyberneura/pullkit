const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const reposEl = document.querySelector("#repos");
const syncButton = document.querySelector("#sync");
const refreshButton = document.querySelector("#refresh");
const selectAll = document.querySelector("#select-all");
const countEl = document.querySelector("#selection-count");
const logEl = document.querySelector("#log");
const runState = document.querySelector("#run-state");
let syncing = false;
// Seeded per page rather than from zero, so a run left over from an earlier
// page cannot reuse a token this page is about to hand out.
let inspectionToken = Date.now();
// A load stays here from the moment it is started until its background
// `git fetch` calls have finished, so that a sync can say what it is waiting
// for rather than stopping on a lock with nothing in the log.
const runningLoads = new Set();

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
  logEl.textContent = `pullkit run: ${names.length} repositories\n\n`;
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
    const results = await invoke("sync_selected", { names });
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

function showCommits(event) {
  const { token, commits } = event.payload;
  if (token !== inspectionToken) return;
  const el = document.querySelector(`.commits[data-repo="${CSS.escape(commits.name)}"]`);
  // The list and the inspection read the config separately, so an edit between
  // the two can point one name at a different directory. Only the row that was
  // drawn for this directory may take the result.
  if (el && el.dataset.path === commits.path) el.innerHTML = commitsHtml(commits);
}

function appendLog(event) {
  logEl.textContent += `${event.payload}\n`;
  logEl.scrollTop = logEl.scrollHeight;
}

// The first load has to wait for the listeners: a fetch that finishes before
// `listen` has registered would leave its row on the placeholder.
Promise.all([listen("commit-inspected", showCommits), listen("sync-log", appendLog)]).then(refreshRepos);
