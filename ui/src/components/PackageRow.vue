<script setup lang="ts">
import { computed, ref } from "vue";
import { api } from "../api";
import type { FileDto, PackageDto, PriorityDto } from "../bindings";
import { etaSeconds, formatBytes, formatEta, formatSpeed, parseRate, percent, rateText } from "../format";
import { useEngineStore } from "../stores/engine";
import { useToastStore } from "../stores/toasts";
import BlockGrid from "./BlockGrid.vue";
import ModalDialog from "./ModalDialog.vue";

const props = defineProps<{ pkg: PackageDto; index: number; count: number }>();

const engine = useEngineStore();
const toasts = useToastStore();

const open = ref(false);
const gridFile = ref<number | null>(null);
const confirmRemove = ref(false);
const deleteFiles = ref(false);
const limitText = ref(rateText(props.pkg.limit));

const speed = computed(() => engine.speedOf(props.pkg));
const pct = computed(() => percent(props.pkg.size, props.pkg.downloaded));
const eta = computed(() =>
  formatEta(etaSeconds(props.pkg.size, props.pkg.downloaded, speed.value)),
);

const STATE_TEXT: Record<PackageDto["state"], string> = {
  queued: "Queued",
  downloading: "Downloading",
  paused: "Paused",
  verifying: "Verifying",
  extracting: "Extracting",
  done: "Done",
  failed: "Failed",
};

const canPause = computed(() => ["queued", "downloading"].includes(props.pkg.state));
const canResume = computed(() => props.pkg.state === "paused");

const statusLine = computed(() => {
  const state = STATE_TEXT[props.pkg.state];
  if (props.pkg.state === "downloading") {
    return `${state} · ${formatSpeed(speed.value)} · ETA ${eta.value}`;
  }
  return props.pkg.error ? `${state}: ${props.pkg.error}` : state;
});

function fileSpeed(file: FileDto): string {
  return file.state === "active" ? formatSpeed(engine.fileSpeeds.get(file.id) ?? 0) : "";
}

function fileStatus(file: FileDto): string {
  if (file.state === "failed") return file.error ?? "Failed";
  if (file.state === "verified" || file.state === "complete") return "Done";
  if (file.state === "active") return `${percent(file.size, file.downloaded) ?? "?"}%`;
  return file.state[0].toUpperCase() + file.state.slice(1);
}

async function setPriority(event: Event) {
  const value = (event.target as HTMLSelectElement).value as PriorityDto;
  await engine.run(() => api.setPriority(props.pkg.id, value));
}

async function setPost(extract: boolean, deleteArchives: boolean) {
  await engine.run(() => api.setPostOptions(props.pkg.id, extract, deleteArchives));
}

async function saveLimit() {
  const parsed = parseRate(limitText.value);
  if (parsed === undefined) {
    toasts.error("A speed limit looks like 500K, 5M or 1.5G. Leave it empty for no limit.");
    return;
  }
  await engine.run(() => api.setPackageLimit(props.pkg.id, parsed));
}

async function remove() {
  confirmRemove.value = false;
  await engine.remove(props.pkg.id, deleteFiles.value);
}

const moveUp = () => engine.run(() => api.move(props.pkg.id, props.index - 1));
const moveDown = () => engine.run(() => api.move(props.pkg.id, props.index + 1));
</script>

<template>
  <li class="pkg" :class="pkg.state">
    <div class="row">
      <button
        class="btn small twisty"
        type="button"
        :aria-expanded="open"
        :aria-label="`${open ? 'Collapse' : 'Expand'} ${pkg.name}`"
        @click="open = !open"
      >
        {{ open ? "▾" : "▸" }}
      </button>
      <div class="main">
        <div class="title">
          <strong class="name">{{ pkg.name }}</strong>
          <span v-if="pkg.priority !== 'normal'" class="tag">{{ pkg.priority }}</span>
          <span class="muted num size">
            {{ formatBytes(pkg.downloaded) }}<template v-if="pkg.size !== null"> / {{ formatBytes(pkg.size) }}</template>
          </span>
        </div>
        <div
          class="bar"
          role="progressbar"
          :aria-label="`Progress of ${pkg.name}`"
          aria-valuemin="0"
          aria-valuemax="100"
          :aria-valuenow="pct ?? undefined"
        >
          <div class="fill" :style="{ width: `${pct ?? (pkg.state === 'done' ? 100 : 0)}%` }" />
        </div>
        <div class="status" :class="{ bad: pkg.state === 'failed' || (pkg.state === 'paused' && pkg.error) }">
          {{ statusLine }}
        </div>
      </div>
      <div class="actions">
        <button v-if="canPause" class="btn small" type="button" :aria-label="`Pause ${pkg.name}`" @click="engine.pause(pkg.id)">
          Pause
        </button>
        <button v-if="canResume" class="btn small" type="button" :aria-label="`Resume ${pkg.name}`" @click="engine.resume(pkg.id)">
          Resume
        </button>
        <button class="btn small" type="button" :disabled="index === 0" :aria-label="`Move ${pkg.name} up`" @click="moveUp">↑</button>
        <button class="btn small" type="button" :disabled="index === count - 1" :aria-label="`Move ${pkg.name} down`" @click="moveDown">↓</button>
        <button class="btn small danger" type="button" :aria-label="`Remove ${pkg.name}`" @click="confirmRemove = true">
          Remove
        </button>
      </div>
    </div>

    <div v-if="open" class="details">
      <div class="options">
        <label>
          Priority
          <select class="field" :value="pkg.priority" @change="setPriority">
            <option value="high">High</option>
            <option value="normal">Normal</option>
            <option value="low">Low</option>
          </select>
        </label>
        <label>
          Speed limit
          <input
            v-model="limitText"
            class="field limit"
            placeholder="none"
            @keydown.enter="saveLimit"
            @blur="saveLimit"
          />
        </label>
        <template v-if="pkg.is_archive">
          <label class="check">
            <input
              type="checkbox"
              :checked="pkg.extract"
              @change="setPost(($event.target as HTMLInputElement).checked, pkg.delete_archives)"
            />
            Extract after download
          </label>
          <label class="check" :class="{ muted: !pkg.extract }">
            <input
              type="checkbox"
              :checked="pkg.delete_archives"
              :disabled="!pkg.extract"
              @change="setPost(pkg.extract, ($event.target as HTMLInputElement).checked)"
            />
            Delete archive after extraction
          </label>
        </template>
      </div>
      <p class="muted dest">Saving to {{ pkg.dest_dir }}</p>

      <ul class="files">
        <li v-for="file in pkg.files" :key="file.id">
          <div class="frow">
            <span class="fname" :title="file.name">{{ file.name }}</span>
            <span class="muted num">
              {{ formatBytes(file.downloaded) }}<template v-if="file.size !== null"> / {{ formatBytes(file.size) }}</template>
            </span>
            <span class="num fspeed">{{ fileSpeed(file) }}</span>
            <span class="fstatus" :class="{ bad: file.state === 'failed' }">{{ fileStatus(file) }}</span>
            <button
              v-if="file.blocks > 0"
              class="btn small"
              type="button"
              :aria-expanded="gridFile === file.id"
              :aria-label="`${gridFile === file.id ? 'Hide' : 'Show'} block map of ${file.name}`"
              @click="gridFile = gridFile === file.id ? null : file.id"
            >
              Blocks
            </button>
            <button
              v-if="file.state === 'failed'"
              class="btn small"
              type="button"
              :aria-label="`Restart ${file.name}`"
              @click="engine.restartFile(file.id)"
            >
              Restart
            </button>
          </div>
          <BlockGrid
            v-if="gridFile === file.id"
            :package-id="pkg.id"
            :file-id="file.id"
            :live="file.state === 'active'"
          />
        </li>
      </ul>
    </div>

    <ModalDialog v-if="confirmRemove" :title="`Remove ${pkg.name}?`" @close="confirmRemove = false">
      <p>This takes the package out of the queue.</p>
      <label class="check">
        <input v-model="deleteFiles" type="checkbox" data-autofocus />
        Also delete the downloaded files from disk
      </label>
      <template #footer>
        <button class="btn" type="button" @click="confirmRemove = false">Cancel</button>
        <button class="btn danger" type="button" @click="remove">Remove</button>
      </template>
    </ModalDialog>
  </li>
</template>

<style scoped>
.pkg {
  list-style: none;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 10px 12px;
}

.row {
  display: grid;
  grid-template-columns: auto 1fr auto;
  gap: 10px;
  align-items: center;
}

.main {
  min-width: 0;
}

.title {
  display: flex;
  gap: 10px;
  align-items: baseline;
}

.name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.size {
  margin-left: auto;
  white-space: nowrap;
}

.tag {
  font-size: 11px;
  text-transform: uppercase;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 0 4px;
}

.bar {
  height: 6px;
  background: var(--surface-2);
  border-radius: 3px;
  overflow: hidden;
  margin: 6px 0 4px;
}

.fill {
  height: 100%;
  background: var(--accent);
  transition: width 0.25s linear;
}

.done .fill {
  background: var(--ok);
}

.failed .fill {
  background: var(--danger);
}

.status {
  font-size: 12px;
  color: var(--muted);
}

.bad {
  color: var(--danger);
}

.actions {
  display: flex;
  gap: 4px;
}

.details {
  margin-top: 10px;
  padding-top: 10px;
  border-top: 1px solid var(--border);
}

.options {
  display: flex;
  flex-wrap: wrap;
  gap: 8px 20px;
  align-items: center;
}

.options label {
  display: flex;
  gap: 6px;
  align-items: center;
}

.limit {
  width: 7em;
}

.dest {
  margin: 8px 0;
  font-size: 12px;
  overflow-wrap: anywhere;
}

.files {
  list-style: none;
  margin: 0;
  padding: 0;
}

.files > li {
  padding: 4px 0;
}

.frow {
  display: grid;
  grid-template-columns: 1fr auto 6.5em 8em auto auto;
  gap: 10px;
  align-items: center;
}

.fname {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.fspeed {
  text-align: right;
}

.fstatus {
  font-size: 12px;
}

@media (max-width: 760px) {
  .frow {
    grid-template-columns: 1fr auto;
  }
}
</style>
