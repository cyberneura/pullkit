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

function selectedNames() {
  return [...document.querySelectorAll(".repo-check:checked")].map((input) => input.value);
}

function updateSelection() {
  const all = [...document.querySelectorAll(".repo-check")];
  const selected = selectedNames().length;
  countEl.textContent = `${selected} selected`;
  syncButton.disabled = syncing || selected === 0;
  selectAll.checked = all.length > 0 && selected === all.length;
  selectAll.indeterminate = selected > 0 && selected < all.length;
}

function statusFor(repo) {
  if (repo.error) return ["error", "Error"];
  if (!repo.clean) return ["dirty", "Dirty"];
  if (!repo.on_main) return ["branch", repo.branch || "Other branch"];
  return ["", "Ready"];
}

async function loadRepos() {
  reposEl.innerHTML = '<p class="empty muted">Loading configuration…</p>';
  try {
    const repos = await invoke("list_repos");
    if (!repos.length) {
      reposEl.innerHTML = '<p class="empty muted">No repositories in config.yaml.</p>';
    } else {
      reposEl.innerHTML = repos.map((repo) => {
        const [kind, text] = statusFor(repo);
        return `<label class="repo" title="${escapeHtml(repo.error || "")}">
          <input class="repo-check" type="checkbox" value="${escapeHtml(repo.name)}" />
          <span class="repo-info"><span class="repo-name">${escapeHtml(repo.name)}</span><span class="repo-path">${escapeHtml(repo.path)}</span></span>
          <span class="status ${kind}">${escapeHtml(text)}</span>
        </label>`;
      }).join("");
    }
    document.querySelectorAll(".repo-check").forEach((el) => el.addEventListener("change", updateSelection));
  } catch (error) {
    reposEl.innerHTML = `<p class="empty status error">${escapeHtml(String(error))}</p>`;
  }
  updateSelection();
}

function escapeHtml(value) {
  return value.replace(/[&<>"']/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[char]);
}

selectAll.addEventListener("change", () => {
  document.querySelectorAll(".repo-check").forEach((el) => { el.checked = selectAll.checked; });
  updateSelection();
});
refreshButton.addEventListener("click", loadRepos);

syncButton.addEventListener("click", async () => {
  const names = selectedNames();
  syncing = true;
  updateSelection();
  logEl.textContent = `pullkit run: ${names.length} repositories\n\n`;
  runState.textContent = "Running";
  runState.className = "badge running";
  try {
    const results = await invoke("sync_selected", { names });
    logEl.textContent += "\nSummary\n";
    results.forEach((result) => { logEl.textContent += `  ${result.name.padEnd(20)} ${result.outcome}: ${result.message}\n`; });
    runState.textContent = "Complete";
    runState.className = "badge done";
    await loadRepos();
  } catch (error) {
    logEl.textContent += `\nERROR ${error}`;
    runState.textContent = "Failed";
    runState.className = "badge";
  } finally {
    syncing = false;
    updateSelection();
  }
});

listen("sync-log", (event) => {
  logEl.textContent += `${event.payload}\n`;
  logEl.scrollTop = logEl.scrollHeight;
});

loadRepos();

