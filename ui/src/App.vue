<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import AddLinks from "./components/AddLinks.vue";
import LinksPanel from "./components/LinksPanel.vue";
import QueueView from "./components/QueueView.vue";
import SettingsView from "./components/SettingsView.vue";
import ToastHost from "./components/ToastHost.vue";
import { formatSpeed } from "./format";
import { useEngineStore } from "./stores/engine";
import { useSettingsStore } from "./stores/settings";

const engine = useEngineStore();
const settings = useSettingsStore();

const TABS = [
  { id: "downloads", label: "Downloads" },
  { id: "links", label: "Links" },
  { id: "settings", label: "Settings" },
] as const;
type TabId = (typeof TABS)[number]["id"];

const tab = ref<TabId>("downloads");
const adding = ref(false);
const pasted = ref("");
const ready = ref(false);
const startError = ref<string | null>(null);

const engineError = computed(() => engine.snapshot.engine_error ?? startError.value);

function openAdd(text = "") {
  pasted.value = text;
  adding.value = true;
  tab.value = "downloads";
}

function onPaste(event: ClipboardEvent) {
  const target = event.target as HTMLElement | null;
  if (adding.value || target?.closest("input, textarea, select, [contenteditable]")) return;
  const text = event.clipboardData?.getData("text") ?? "";
  if (/https?:\/\/|ftp:\/\//i.test(text)) {
    event.preventDefault();
    openAdd(text);
  }
}

function onKey(event: KeyboardEvent) {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "n" && !adding.value) {
    event.preventDefault();
    openAdd();
  }
}

function onTabKey(event: KeyboardEvent) {
  const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
  if (step === 0) return;
  const index = TABS.findIndex((t) => t.id === tab.value);
  const next = TABS[(index + step + TABS.length) % TABS.length];
  tab.value = next.id;
  document.getElementById(`tab-${next.id}`)?.focus();
}

onMounted(async () => {
  window.addEventListener("paste", onPaste);
  window.addEventListener("keydown", onKey);
  try {
    await settings.load();
    await engine.start();
  } catch (error) {
    startError.value = error instanceof Error ? error.message : String(error);
  }
  ready.value = true;
});

onBeforeUnmount(() => {
  window.removeEventListener("paste", onPaste);
  window.removeEventListener("keydown", onKey);
});
</script>

<template>
  <div class="app">
    <header class="top">
      <h1>Nux Downloader</h1>
      <div class="speed num" aria-live="off">
        <span class="muted">Speed</span> <strong>{{ formatSpeed(engine.totalSpeed) }}</strong>
      </div>
      <div class="buttons">
        <button class="btn primary" type="button" :disabled="!!engineError" @click="openAdd()">
          Add links
        </button>
        <button class="btn" type="button" :disabled="!engine.hasRunning" @click="engine.pause(null)">
          Pause all
        </button>
        <button class="btn" type="button" :disabled="!engine.hasPaused" @click="engine.resume(null)">
          Resume all
        </button>
      </div>
    </header>

    <div role="tablist" aria-label="Sections" class="tabs" @keydown="onTabKey">
      <button
        v-for="t in TABS"
        :id="`tab-${t.id}`"
        :key="t.id"
        role="tab"
        type="button"
        :aria-selected="tab === t.id"
        :aria-controls="`panel-${t.id}`"
        :tabindex="tab === t.id ? 0 : -1"
        class="tab"
        :class="{ on: tab === t.id }"
        @click="tab = t.id"
      >
        {{ t.label }}
      </button>
    </div>

    <div v-if="settings.restartNeeded" class="banner" role="status">
      Some settings apply after the engine restarts.
      <button class="btn small" type="button" @click="settings.restartEngine()">Restart engine</button>
    </div>
    <div v-if="engineError" class="banner error" role="alert">
      The download engine isn't running: {{ engineError }}
    </div>

    <main>
      <div
        v-if="ready"
        :id="`panel-${tab}`"
        role="tabpanel"
        :aria-labelledby="`tab-${tab}`"
      >
        <QueueView v-if="tab === 'downloads'" />
        <LinksPanel v-else-if="tab === 'links'" />
        <SettingsView v-else />
      </div>
    </main>

    <AddLinks v-if="adding" :initial-text="pasted" @close="adding = false" />
    <ToastHost />
  </div>
</template>

<style scoped>
.app {
  max-width: 1100px;
  margin: 0 auto;
  padding: 0 16px 32px;
}

.top {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 16px;
  align-items: center;
  padding: 14px 0 8px;
}

h1 {
  font-size: 18px;
  margin: 0;
  margin-right: auto;
}

.buttons {
  display: flex;
  gap: 6px;
}

.tabs {
  display: flex;
  gap: 2px;
  border-bottom: 1px solid var(--border);
  margin-bottom: 12px;
}

.tab {
  background: none;
  border: 0;
  border-bottom: 3px solid transparent;
  padding: 8px 14px;
  cursor: pointer;
  color: var(--muted);
}

.tab.on {
  color: var(--fg);
  border-bottom-color: var(--accent);
  font-weight: 600;
}

.banner {
  display: flex;
  gap: 12px;
  align-items: center;
  padding: 8px 12px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--surface-2);
  margin-bottom: 12px;
}

.banner.error {
  border-color: var(--danger);
  color: var(--danger);
}
</style>
