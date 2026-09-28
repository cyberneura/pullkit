// The Third-Party Licenses window: pullkit's own license and the notices of the
// libraries compiled into it, both embedded in the binary.
const { invoke } = window.__TAURI__.core;

window.addEventListener("DOMContentLoaded", async () => {
  const text = document.getElementById("licenses-text");
  try {
    text.textContent = await invoke("third_party_notices");
  } catch (error) {
    text.textContent = `Could not load the licenses: ${error}`;
  }
});

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    window.__TAURI__.window.getCurrentWindow().close();
  }
});
