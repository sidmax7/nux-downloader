import { defineStore } from "pinia";
import { ref } from "vue";
import { api } from "../api";
import { events, type LinkPref, type Settings, type UpdateDto } from "../bindings";
import { useToastStore } from "./toasts";

export type Theme = "system" | "light" | "dark";

function readTheme(): Theme {
  try {
    const stored = localStorage.getItem("nuxd-theme");
    if (stored === "light" || stored === "dark") return stored;
  } catch {
    // storage can be unavailable; the system theme is the fallback
  }
  return "system";
}

export function applyTheme(theme: Theme) {
  const root = document.documentElement;
  if (theme === "system") delete root.dataset.theme;
  else root.dataset.theme = theme;
}

export const useSettingsStore = defineStore("settings", () => {
  const toasts = useToastStore();
  const settings = ref<Settings | null>(null);
  const restartNeeded = ref(false);
  const theme = ref<Theme>(readTheme());
  const update = ref<UpdateDto | null>(null);
  const installing = ref(false);

  async function listenForUpdates() {
    await events.updateEvent.listen((event) => {
      update.value = event.payload;
    });
  }

  async function checkNow(): Promise<UpdateDto | null> {
    try {
      update.value = await api.checkForUpdate();
      if (!update.value) toasts.info("You have the latest version.");
      return update.value;
    } catch (error) {
      toasts.error(`Couldn't check for updates: ${error instanceof Error ? error.message : String(error)}`);
      return null;
    }
  }

  async function installUpdate() {
    installing.value = true;
    try {
      await api.installUpdate();
    } catch (error) {
      toasts.error(error instanceof Error ? error.message : String(error));
    } finally {
      installing.value = false;
    }
  }

  async function load() {
    settings.value = await api.getSettings();
    applyTheme(theme.value);
  }

  function setTheme(next: Theme) {
    theme.value = next;
    applyTheme(next);
    try {
      if (next === "system") localStorage.removeItem("nuxd-theme");
      else localStorage.setItem("nuxd-theme", next);
    } catch {
      // not worth failing over
    }
  }

  /** Saves; returns whether it worked (a failure is already a toast). */
  async function save(next: Settings): Promise<boolean> {
    try {
      const result = await api.updateSettings(next);
      settings.value = result.settings;
      if (result.restart_needed) restartNeeded.value = true;
      return true;
    } catch (error) {
      toasts.error(error instanceof Error ? error.message : String(error));
      return false;
    }
  }

  /** Changes one link's saved choices. */
  async function setLinkPref(name: string, change: Partial<LinkPref>) {
    if (!settings.value) return;
    const before = settings.value.link_prefs[name] ?? {
      label: null,
      color: null,
      enabled: true,
      limit: null,
    };
    await save({
      ...settings.value,
      link_prefs: { ...settings.value.link_prefs, [name]: { ...before, ...change } },
    });
  }

  async function restartEngine() {
    try {
      await api.restartEngine();
      restartNeeded.value = false;
      toasts.info("Engine restarted.");
    } catch (error) {
      toasts.error(error instanceof Error ? error.message : String(error));
    }
  }

  return {
    settings,
    restartNeeded,
    theme,
    update,
    installing,
    load,
    save,
    setLinkPref,
    setTheme,
    restartEngine,
    listenForUpdates,
    checkNow,
    installUpdate,
  };
});
