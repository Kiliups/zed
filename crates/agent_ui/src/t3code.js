// Runs in T3 Code's web view before the page does. `t3code.rs` declares `setup`, which is
// `{ theme, workspaceRoot, projectKey, targetPath }`.

// T3 Code reads its theme from local storage as it starts, and the web view's storage is Zed's
// own, so this leaves the T3 Code app's themes alone. Without a matching VS Code theme, the
// previous Zed theme is removed and T3 Code uses its system theme.
try {
  const themes = JSON.parse(localStorage.getItem("t3code:themes:v1") ?? "[]");
  if (setup.theme) {
    localStorage.setItem(
      "t3code:themes:v1",
      JSON.stringify([...themes.filter(({ id }) => id !== setup.theme.id), setup.theme]),
    );
    localStorage.setItem("t3code:theme", setup.theme.id);
    localStorage.setItem("t3code:theme-appearance-mode", setup.theme.appearance);
  } else {
    const remaining = themes.filter(({ id }) => id !== "zed");
    if (remaining.length !== themes.length) {
      localStorage.setItem("t3code:themes:v1", JSON.stringify(remaining));
    }
    if (localStorage.getItem("t3code:theme") === "zed") {
      localStorage.removeItem("t3code:theme");
      localStorage.removeItem("t3code:theme-appearance-mode");
    }
  }
} catch {}

// New threads start in the last used project. The value is a scoped project key, and T3 Code's
// hooks only notice writes that announce themselves.
if (setup.projectKey) {
  try {
    const key = "t3code:last-used-project:v1";
    localStorage.setItem(key, JSON.stringify(setup.projectKey));
    window.dispatchEvent(new StorageEvent("storage", { key }));
    window.dispatchEvent(
      new CustomEvent("t3code:local_storage_change", { detail: { key } }),
    );
  } catch {}
}

const isAppPage = () =>
  location.pathname !== "/pair" && !location.pathname.startsWith("/welcome");

// Without a thread to open, T3 Code lands on a draft in the project with the latest activity, so
// the draft's project picker moves it to the workspace's project. The picker only shows project
// titles and can't be closed from a script, so it is only opened when exactly one project has the
// workspace project's title. Only the first draft after pairing is moved, so the user's own picks
// stay.
if (!setup.targetPath && setup.workspaceRoot) {
  (async () => {
    while (!location.pathname.startsWith("/draft/")) {
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    if (sessionStorage.getItem("zed:t3-project-picked") === "1") return;
    sessionStorage.setItem("zed:t3-project-picked", "1");
    const { projects } = await (await fetch("/api/projects")).json();
    const active = projects.filter((project) => !project.archivedAt);
    const project = active.find((project) => project.workspaceRoot === setup.workspaceRoot);
    if (!project || active.filter(({ title }) => title === project.title).length !== 1) return;

    const deadline = Date.now() + 10_000;
    while (Date.now() < deadline) {
      const current = document
        .querySelector('button[aria-label^="New thread in "]')
        ?.getAttribute("aria-label")
        .slice("New thread in ".length);
      if (current === project.title) return;
      const option = [...document.querySelectorAll('[role="dialog"] [role="option"]')].find(
        (option) => option.innerText.trim().split("\n").at(-1) === project.title,
      );
      if (option) {
        option.click();
        return;
      }
      [...document.querySelectorAll('button[aria-haspopup="dialog"]')]
        .find((button) => current !== undefined && button.innerText.trim() === current)
        ?.click();
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  })().catch(() => {});
}

// The pairing page redirects to "/" after it signs in. Once the app is up, open the requested
// thread.
if (setup.targetPath) {
  const timer = setInterval(() => {
    if (!isAppPage() || location.pathname === setup.targetPath) {
      if (location.pathname === setup.targetPath) clearInterval(timer);
      return;
    }
    // Only the first page after pairing navigates, so a reload stays on the thread
    // the user picked inside T3 Code.
    if (sessionStorage.getItem("zed:t3-navigated") === "1") {
      clearInterval(timer);
      return;
    }
    sessionStorage.setItem("zed:t3-navigated", "1");
    location.assign(setup.targetPath);
  }, 250);
  setTimeout(() => clearInterval(timer), 30_000);
}
