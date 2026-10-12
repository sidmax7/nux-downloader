<script setup lang="ts">
import { onMounted, reactive, ref } from "vue";
import type { AboutDto } from "../bindings";
import { api } from "../api";
import type { Settings } from "../bindings";
import { parseRate, rateText } from "../format";
import { type Theme, useSettingsStore } from "../stores/settings";
import { useToastStore } from "../stores/toasts";
import ModalDialog from "./ModalDialog.vue";

const store = useSettingsStore();
const toasts = useToastStore();

const form = reactive({
  downloadDir: "",
  globalLimit: "",
  slots: 8,
  total: 32,
  autoResume: true,
  extractByDefault: true,
  deleteByDefault: false,
  hostRules: "",
  notifications: true,
  closeToTray: false,
  checkUpdates: true,
});
const about = ref<AboutDto | null>(null);
const notices = ref<string | null>(null);
const checking = ref(false);
const report = ref<string | null>(null);
const saving = ref(false);

function fill(settings: Settings) {
  form.downloadDir = settings.download_dir ?? "";
  form.globalLimit = rateText(settings.global_limit);
  form.slots = settings.slots_per_link;
  form.total = settings.max_connections_total;
  form.autoResume = settings.auto_resume;
  form.extractByDefault = settings.extract_by_default;
  form.deleteByDefault = settings.delete_archives_by_default;
  form.hostRules = settings.host_rules.join("\n");
  form.notifications = settings.notifications;
  form.closeToTray = settings.close_to_tray;
  form.checkUpdates = settings.check_updates;
}

onMounted(async () => {
  if (store.settings) fill(store.settings);
  about.value = await api.about();
});

async function checkNow() {
  checking.value = true;
  await store.checkNow();
  checking.value = false;
}

async function showNotices() {
  notices.value = await api.notices();
}

async function browse() {
  const picked = await api.pickFolder();
  if (picked) form.downloadDir = picked;
}

async function save() {
  if (!store.settings) return;
  const limit = parseRate(form.globalLimit);
  if (limit === undefined) {
    toasts.error("The overall speed limit looks like 500K, 5M or 1.5G. Leave it empty for none.");
    return;
  }
  saving.value = true;
  const ok = await store.save({
    ...store.settings,
    download_dir: form.downloadDir.trim() || null,
    global_limit: limit,
    slots_per_link: Number(form.slots),
    max_connections_total: Number(form.total),
    auto_resume: form.autoResume,
    extract_by_default: form.extractByDefault,
    delete_archives_by_default: form.deleteByDefault,
    host_rules: form.hostRules
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean),
    notifications: form.notifications,
    close_to_tray: form.closeToTray,
    check_updates: form.checkUpdates,
  });
  saving.value = false;
  if (ok) toasts.info("Settings saved.");
}

async function showReport() {
  try {
    report.value = await api.diagnostics();
  } catch (error) {
    toasts.error(error instanceof Error ? error.message : String(error));
  }
}

async function copyReport() {
  if (!report.value) return;
  try {
    await navigator.clipboard.writeText(report.value);
    toasts.info("Copied.");
  } catch {
    toasts.error("Couldn't copy; select the text and copy it by hand.");
  }
}
</script>

<template>
  <form class="settings" aria-label="Settings" @submit.prevent="save">
    <fieldset>
      <legend>Downloads</legend>
      <label class="line">
        Download folder
        <span class="pair">
          <input v-model="form.downloadDir" class="field" placeholder="Your Downloads folder" />
          <button class="btn" type="button" @click="browse">Browse…</button>
        </span>
      </label>
      <label class="line">
        Overall speed limit
        <input v-model="form.globalLimit" class="field short" placeholder="none" />
      </label>
      <label class="check"><input v-model="form.autoResume" type="checkbox" /> Resume unfinished downloads when the app starts <small class="muted">(applies after a restart)</small></label>
    </fieldset>

    <fieldset>
      <legend>Archives</legend>
      <label class="check"><input v-model="form.extractByDefault" type="checkbox" /> Tick “Extract after download” for archives</label>
      <label class="check"><input v-model="form.deleteByDefault" type="checkbox" /> Tick “Delete archive after extraction”</label>
    </fieldset>

    <fieldset>
      <legend>Connections <small class="muted">(apply after a restart)</small></legend>
      <label class="line">
        Per network link
        <input v-model.number="form.slots" type="number" min="1" max="32" class="field short" />
      </label>
      <label class="line">
        In total
        <input v-model.number="form.total" type="number" min="1" max="512" class="field short" />
      </label>
      <label class="line top">
        Host rules
        <span class="stack">
          <textarea v-model="form.hostRules" class="field" rows="3" spellcheck="false" placeholder="*.example.com=4:2" />
          <small class="muted">One per line: <code>PATTERN=CONNECTIONS[:FILES]</code>. Connections are per link; files is how many files may download from the host at once.</small>
        </span>
      </label>
    </fieldset>

    <fieldset>
      <legend>App</legend>
      <label class="line">
        Theme
        <select
          class="field short"
          :value="store.theme"
          @change="store.setTheme(($event.target as HTMLSelectElement).value as Theme)"
        >
          <option value="system">Match system</option>
          <option value="light">Light</option>
          <option value="dark">Dark</option>
        </select>
      </label>
      <label class="check"><input v-model="form.notifications" type="checkbox" /> Notify when a download finishes or fails</label>
      <label class="check"><input v-model="form.closeToTray" type="checkbox" /> Keep running in the tray when the window is closed</label>
    </fieldset>

    <fieldset>
      <legend>About</legend>
      <p class="about">
        <strong>Nux Downloader</strong>
        <span v-if="about" class="muted num">version {{ about.version }}</span>
      </p>
      <template v-if="about?.can_self_update">
        <label class="check"><input v-model="form.checkUpdates" type="checkbox"> Check for updates when the app starts</label>
        <div class="row">
          <button class="btn" type="button" :disabled="checking" @click="checkNow">
            {{ checking ? "Checking…" : "Check for updates now" }}
          </button>
          <span v-if="store.update" class="muted">Version {{ store.update.version }} is available.</span>
        </div>
      </template>
      <p v-else-if="about" class="muted small">
        Updates for this copy come from your package manager.
      </p>
      <div class="row">
        <button class="btn" type="button" @click="showNotices">Third-party licences…</button>
        <span class="muted small">Licensed under Apache-2.0.</span>
      </div>
    </fieldset>

    <div class="buttons">
      <button class="btn primary" type="submit" :disabled="saving">Save settings</button>
      <button class="btn" type="button" @click="showReport">Diagnostics…</button>
    </div>

    <ModalDialog v-if="notices !== null" title="Third-party licences" wide @close="notices = null">
      <pre class="report">{{ notices }}</pre>
      <template #footer>
        <button class="btn primary" type="button" @click="notices = null">Close</button>
      </template>
    </ModalDialog>

    <ModalDialog v-if="report !== null" title="Diagnostics" wide @close="report = null">
      <p class="muted">Links in this report are cut down to host and path. Nothing is sent anywhere.</p>
      <pre class="report">{{ report }}</pre>
      <template #footer>
        <button class="btn" type="button" @click="copyReport">Copy</button>
        <button class="btn primary" type="button" @click="report = null">Close</button>
      </template>
    </ModalDialog>
  </form>
</template>

<style scoped>
.settings {
  display: grid;
  gap: 12px;
  max-width: 760px;
}

fieldset {
  border: 1px solid var(--border);
  background: var(--surface);
  border-radius: 10px;
  padding: 10px 14px 14px;
  display: grid;
  gap: 10px;
}

legend {
  padding: 0 6px;
  font-weight: 600;
}

.line {
  display: grid;
  grid-template-columns: 11em 1fr;
  gap: 10px;
  align-items: center;
}

.line.top {
  align-items: start;
}

.pair {
  display: flex;
  gap: 8px;
}

.pair .field {
  flex: 1;
}

.stack {
  display: grid;
  gap: 4px;
}

.short {
  width: 11em;
}

.about {
  display: flex;
  gap: 10px;
  align-items: baseline;
  margin: 0;
}

.row {
  display: flex;
  gap: 12px;
  align-items: center;
  flex-wrap: wrap;
}

.small {
  font-size: 12.5px;
}

.check {
  display: flex;
  gap: 8px;
  align-items: center;
}

.buttons {
  display: flex;
  gap: 8px;
  position: sticky;
  bottom: -32px;
  margin: 0 -24px -32px;
  padding: 12px 24px;
  background: color-mix(in srgb, var(--bg) 92%, transparent);
  border-top: 1px solid var(--border);
}

.report {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  font-size: 12px;
  margin: 0;
}

@media (max-width: 600px) {
  .line {
    grid-template-columns: 1fr;
  }
}
</style>
