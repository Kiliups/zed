// Runs in T3 Code's web view before the page does. `t3code.rs` declares `setup`, which is
// `{ theme, workspaceRoot, projectName, projectKey, targetPath }`.

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

// The pairing page redirects to "/" after it signs in. Once the app is up, open the requested
// thread, or start a draft in the workspace's project when it has no threads yet.
(() => {
  const isAppPage = () =>
    location.pathname !== "/pair" && !location.pathname.startsWith("/welcome");

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
    return;
  }

  if (!setup.projectName) return;
  const label = `New thread in ${setup.projectName}`;
  const timer = setInterval(() => {
    if (!isAppPage() || sessionStorage.getItem("zed:t3-started") === "1") return;
    const buttons = [...document.querySelectorAll("button[aria-label]")];
    const button =
      buttons.find((candidate) => candidate.getAttribute("aria-label") === label) ??
      buttons.find((candidate) =>
        candidate
          .getAttribute("aria-label")
          .toLowerCase()
          .includes(setup.projectName.toLowerCase()),
      );
    if (button) {
      sessionStorage.setItem("zed:t3-started", "1");
      button.click();
      clearInterval(timer);
    }
  }, 500);
  setTimeout(() => clearInterval(timer), 30_000);
})();
