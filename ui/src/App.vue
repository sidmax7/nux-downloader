<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import AddLinks from "./components/AddLinks.vue";
import UiIcon from "./components/UiIcon.vue";
import type { IconName } from "./icons";
import LinksPanel from "./components/LinksPanel.vue";
import QueueView from "./components/QueueView.vue";
import SettingsView from "./components/SettingsView.vue";
import ThroughputBar from "./components/ThroughputBar.vue";
import ToastHost from "./components/ToastHost.vue";
import { formatSpeed } from "./format";
import { useEngineStore } from "./stores/engine";
import { useSettingsStore } from "./stores/settings";

const engine = useEngineStore();
const settings = useSettingsStore();

const TABS: { id: "downloads" | "links" | "settings"; label: string; icon: IconName }[] = [
  { id: "downloads", label: "Downloads", icon: "download" },
  { id: "links", label: "Links", icon: "links" },
  { id: "settings", label: "Settings", icon: "sliders" },
];
type TabId = (typeof TABS)[number]["id"];

function tabFromHash(): TabId {
  const wanted = window.location.hash.slice(1);
  return TABS.find((t) => t.id === wanted)?.id ?? "downloads";
}

const tab = ref<TabId>(tabFromHash());
const adding = ref(false);
const pasted = ref("");
const ready = ref(false);
const startError = ref<string | null>(null);

const engineError = computed(() => engine.snapshot.engine_error ?? startError.value);
const active = computed(() => engine.packages.filter((p) => p.state === "downloading").length);
const liveLinks = computed(
  () => engine.links.filter((link) => link.enabled && link.active > 0).length,
);
const title = computed(() => TABS.find((t) => t.id === tab.value)?.label ?? "");

watch(tab, (id) => history.replaceState(null, "", `#${id}`));

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
  const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
  if (step === 0) return;
  event.preventDefault();
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
  <div class="shell">
    <aside class="rail">
      <div class="brand">
        <svg class="mark" viewBox="0 0 32 32" width="26" height="26" aria-hidden="true">
          <rect width="32" height="32" rx="8" fill="var(--accent)" />
          <path
            d="M9 10.5h14M9 16h14M9 21.5h14"
            stroke="var(--accent-fg)"
            stroke-width="2.4"
            stroke-linecap="round"
            opacity="0.45"
          />
          <path
            d="M9 10.5h9M9 16h14M9 21.5h6"
            stroke="var(--accent-fg)"
            stroke-width="2.4"
            stroke-linecap="round"
          />
        </svg>
        <span class="word">Nux</span>
      </div>

      <nav role="tablist" aria-label="Sections" aria-orientation="vertical" class="tabs" @keydown="onTabKey">
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
          <UiIcon :name="t.icon" :size="17" />
          <span class="label">{{ t.label }}</span>
          <span v-if="t.id === 'downloads' && active > 0" class="badge num">{{ active }}</span>
        </button>
      </nav>

      <div class="meter">
        <div class="readout">
          <strong class="speed num">{{ formatSpeed(engine.totalSpeed) }}</strong>
          <span class="muted">
            {{ liveLinks === 0 ? "idle" : `${liveLinks} link${liveLinks === 1 ? "" : "s"}` }}
          </span>
        </div>
        <ThroughputBar :links="engine.links" />
      </div>
    </aside>

    <div class="content">
      <header class="bar">
        <h1>{{ title }}</h1>
        <div class="actions">
          <button v-if="tab === 'downloads'" class="btn" type="button" :disabled="!engine.hasRunning" @click="engine.pause(null)">
            <UiIcon name="pause" /> Pause all
          </button>
          <button v-if="tab === 'downloads'" class="btn" type="button" :disabled="!engine.hasPaused" @click="engine.resume(null)">
            <UiIcon name="play" /> Resume all
          </button>
          <button class="btn primary" type="button" :disabled="!!engineError" @click="openAdd()">
            <UiIcon name="plus" /> Add links
          </button>
        </div>
      </header>

      <div v-if="settings.restartNeeded" class="banner" role="status">
        <UiIcon name="restart" />
        <span>Some settings apply after the engine restarts.</span>
        <button class="btn small" type="button" @click="settings.restartEngine()">Restart engine</button>
      </div>
      <div v-if="engineError" class="banner error" role="alert">
        <UiIcon name="alert" />
        <span>The download engine isn't running: {{ engineError }}</span>
      </div>

      <main>
        <div v-if="ready" :id="`panel-${tab}`" role="tabpanel" :aria-labelledby="`tab-${tab}`">
          <QueueView v-if="tab === 'downloads'" @add="openAdd()" />
          <LinksPanel v-else-if="tab === 'links'" />
          <SettingsView v-else />
        </div>
        <div v-else class="skeleton" aria-busy="true" aria-label="Loading">
          <div v-for="n in 3" :key="n" class="sk-row" />
        </div>
      </main>
    </div>

    <AddLinks v-if="adding" :initial-text="pasted" @close="adding = false" />
    <ToastHost />
  </div>
</template>

<style scoped>
.shell {
  display: grid;
  grid-template-columns: 208px minmax(0, 1fr);
  height: 100vh;
}

.rail {
  display: flex;
  flex-direction: column;
  gap: 18px;
  padding: 16px 12px;
  background: var(--rail);
  border-right: 1px solid var(--border);
  min-height: 0;
}

.brand {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 2px 6px;
}

.word {
  font-size: 16px;
  font-weight: 700;
  letter-spacing: -0.01em;
}

.tabs {
  display: grid;
  gap: 2px;
}

.tab {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border: 0;
  border-radius: 7px;
  background: none;
  color: var(--muted);
  cursor: pointer;
  text-align: left;
  transition:
    background-color 0.15s ease-out,
    color 0.15s ease-out;
}

.tab:hover {
  background: var(--surface-2);
  color: var(--fg);
}

.tab.on {
  background: var(--accent-soft);
  color: var(--accent);
  font-weight: 600;
}

.label {
  flex: 1;
}

.badge {
  font-size: 11.5px;
  font-weight: 600;
  min-width: 20px;
  text-align: center;
  padding: 0 6px;
  border-radius: 10px;
  background: var(--accent);
  color: var(--accent-fg);
}

.meter {
  margin-top: auto;
  padding: 12px;
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--surface);
  display: grid;
  gap: 9px;
}

.readout {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 8px;
}

.speed {
  font-size: 15px;
}

.content {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}

.bar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 24px;
  border-bottom: 1px solid var(--border);
}

h1 {
  font-size: 17px;
  margin: 0 auto 0 0;
  letter-spacing: -0.01em;
}

.actions {
  display: flex;
  gap: 8px;
}

main {
  flex: 1;
  min-height: 0;
  overflow: auto;
  padding: 18px 24px 32px;
}

.banner {
  display: flex;
  gap: 10px;
  align-items: center;
  margin: 12px 24px 0;
  padding: 9px 12px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--surface);
}

.banner span {
  flex: 1;
}

.banner.error {
  border-color: var(--danger);
  color: var(--danger);
}

.skeleton {
  display: grid;
  gap: 8px;
}

.sk-row {
  height: 66px;
  border-radius: 10px;
  background: var(--surface);
  border: 1px solid var(--border);
  animation: pulse 1.4s ease-in-out infinite;
}

@keyframes pulse {
  50% {
    opacity: 0.55;
  }
}

@media (max-width: 760px) {
  .shell {
    grid-template-columns: 1fr;
    grid-template-rows: auto minmax(0, 1fr);
  }

  .rail {
    flex-direction: row;
    align-items: center;
    padding: 8px 12px;
    border-right: 0;
    border-bottom: 1px solid var(--border);
    gap: 10px;
  }

  .word,
  .meter {
    display: none;
  }

  /* Icon-only rail, but the names stay available to screen readers. */
  .tab .label {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }

  .tabs {
    display: flex;
  }

  .bar {
    flex-wrap: wrap;
    padding: 12px 16px;
  }

  main {
    padding: 14px 16px 24px;
  }
}
</style>
