<script setup lang="ts">
import { computed, ref } from "vue";
import { api } from "../api";
import type { FileDto, PackageDto, PriorityDto } from "../bindings";
import { etaSeconds, formatBytes, formatEta, formatSpeed, parseRate, percent, rateText } from "../format";
import { useEngineStore } from "../stores/engine";
import { useToastStore } from "../stores/toasts";
import BlockGrid from "./BlockGrid.vue";
import UiIcon from "./UiIcon.vue";
import type { IconName } from "../icons";
import ModalDialog from "./ModalDialog.vue";

const props = defineProps<{ pkg: PackageDto; index: number; count: number; first?: boolean }>();

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

const STATE_ICON: Record<PackageDto["state"], IconName> = {
  queued: "clock",
  downloading: "download",
  paused: "pause",
  verifying: "check",
  extracting: "archive",
  done: "check",
  failed: "alert",
};

const canPause = computed(() => ["queued", "downloading"].includes(props.pkg.state));
const canResume = computed(() => props.pkg.state === "paused");

const statusLine = computed(() => {
  const state = STATE_TEXT[props.pkg.state];
  return props.pkg.error ? `${state}: ${props.pkg.error}` : state;
});

const hasProblem = computed(
  () => props.pkg.state === "failed" || (props.pkg.state === "paused" && !!props.pkg.error),
);

function fileSpeed(file: FileDto): string {
  return file.state === "active" ? formatSpeed(engine.fileSpeedOf(file)) : "";
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
  <li class="pkg" :class="[pkg.state, { open }]">
    <div class="row">
      <button
        class="icon-btn twisty"
        type="button"
        :aria-expanded="open"
        :aria-label="`${open ? 'Collapse' : 'Expand'} ${pkg.name}`"
        @click="open = !open"
      >
        <UiIcon :name="open ? 'chevronDown' : 'chevronRight'" />
      </button>

      <div class="main">
        <div class="title">
          <strong class="name" :title="pkg.name">{{ pkg.name }}</strong>
          <span v-if="pkg.priority !== 'normal'" class="tag">{{ pkg.priority }}</span>
        </div>
        <div
          class="bar"
          role="progressbar"
          :aria-label="`Progress of ${pkg.name}`"
          aria-valuemin="0"
          aria-valuemax="100"
          :aria-valuenow="pct ?? undefined"
        >
          <div class="fill" :style="{ '--p': (pct ?? (pkg.state === 'done' ? 100 : 0)) / 100 }" />
        </div>
        <div class="meta num">
          <span class="state" :class="{ bad: hasProblem }">
            <UiIcon :name="STATE_ICON[pkg.state]" :size="13" />
            {{ statusLine }}
          </span>
          <span class="muted size">
            {{ formatBytes(pkg.downloaded) }}<template v-if="pkg.size !== null"> of {{ formatBytes(pkg.size) }}</template>
            <template v-if="pct !== null && pkg.state !== 'done'"> · {{ pct }}%</template>
          </span>
        </div>
      </div>

      <div v-if="pkg.state === 'downloading'" class="live num">
        <strong>{{ speed > 0 ? formatSpeed(speed) : "–" }}</strong>
        <span class="muted">{{ eta === "–" ? "starting" : `${eta} left` }}</span>
      </div>

      <div class="actions">
        <button
          v-if="canPause"
          class="icon-btn"
          type="button"
          :title="`Pause ${pkg.name}`"
          :aria-label="`Pause ${pkg.name}`"
          @click="engine.pause(pkg.id)"
        >
          <UiIcon name="pause" />
        </button>
        <button
          v-if="canResume"
          class="icon-btn"
          type="button"
          :title="`Resume ${pkg.name}`"
          :aria-label="`Resume ${pkg.name}`"
          @click="engine.resume(pkg.id)"
        >
          <UiIcon name="play" />
        </button>
        <button
          class="icon-btn danger"
          type="button"
          :title="`Remove ${pkg.name}`"
          :aria-label="`Remove ${pkg.name}`"
          @click="confirmRemove = true"
        >
          <UiIcon name="trash" />
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
        <span class="order">
          Queue position
          <button
            class="icon-btn"
            type="button"
            :disabled="index === 0"
            :aria-label="`Move ${pkg.name} up`"
            @click="moveUp"
          >
            <UiIcon name="up" />
          </button>
          <button
            class="icon-btn"
            type="button"
            :disabled="index === count - 1"
            :aria-label="`Move ${pkg.name} down`"
            @click="moveDown"
          >
            <UiIcon name="down" />
          </button>
        </span>
        <template v-if="pkg.is_archive">
          <label class="check">
            <input
              type="checkbox"
              :checked="pkg.extract"
              @change="setPost(($event.target as HTMLInputElement).checked, pkg.delete_archives)"
            >
            Extract after download
          </label>
          <label class="check" :class="{ muted: !pkg.extract }">
            <input
              type="checkbox"
              :checked="pkg.delete_archives"
              :disabled="!pkg.extract"
              @change="setPost(pkg.extract, ($event.target as HTMLInputElement).checked)"
            >
            Delete archive after extraction
          </label>
        </template>
      </div>
      <p class="muted dest">Saving to <span>{{ pkg.dest_dir }}</span></p>

      <ul class="files">
        <li v-for="file in pkg.files" :key="file.id">
          <div class="frow">
            <span class="fname" :title="file.name">{{ file.name }}</span>
            <span class="muted num fsize">
              {{ formatBytes(file.downloaded) }}<template v-if="file.size !== null"> / {{ formatBytes(file.size) }}</template>
            </span>
            <span class="num fspeed">{{ fileSpeed(file) }}</span>
            <span class="fstatus" :class="{ bad: file.state === 'failed' }">{{ fileStatus(file) }}</span>
            <span class="fbtns">
              <button
                v-if="file.blocks > 0"
                class="icon-btn"
                type="button"
                :class="{ on: gridFile === file.id }"
                :aria-expanded="gridFile === file.id"
                :title="`Block map of ${file.name}`"
                :aria-label="`${gridFile === file.id ? 'Hide' : 'Show'} block map of ${file.name}`"
                @click="gridFile = gridFile === file.id ? null : file.id"
              >
                <UiIcon name="blocks" />
              </button>
              <button
                v-if="file.state === 'failed'"
                class="icon-btn"
                type="button"
                :title="`Restart ${file.name}`"
                :aria-label="`Restart ${file.name}`"
                @click="engine.restartFile(file.id)"
              >
                <UiIcon name="restart" />
              </button>
            </span>
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
        <input v-model="deleteFiles" type="checkbox" data-autofocus>
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
  border-radius: 10px;
  padding: 10px 12px 10px 6px;
  transition: border-color 0.15s ease-out;
}

.pkg:hover,
.pkg.open {
  border-color: color-mix(in srgb, var(--border), var(--fg) 18%);
}

.row {
  display: grid;
  grid-template-columns: auto minmax(0, 1fr) auto auto;
  gap: 6px 12px;
  align-items: center;
}

.main {
  min-width: 0;
}

.title {
  display: flex;
  gap: 8px;
  align-items: center;
}

.name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
}

.tag {
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--muted);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 0 5px;
}

.bar {
  height: 5px;
  background: var(--track);
  border-radius: 3px;
  overflow: hidden;
  margin: 7px 0 6px;
}

.fill {
  width: 100%;
  height: 100%;
  border-radius: 3px;
  background: var(--accent);
  transform-origin: left;
  transform: scaleX(var(--p, 0));
  transition: transform 0.25s linear;
}

.done .fill {
  background: var(--ok);
}

.failed .fill {
  background: var(--danger);
}

.paused .fill,
.queued .fill {
  background: color-mix(in srgb, var(--muted) 60%, var(--track));
}

.meta {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  gap: 2px 12px;
  font-size: 12.5px;
}

.state {
  display: inline-flex;
  gap: 5px;
  align-items: center;
  color: var(--muted);
  min-width: 0;
  overflow-wrap: break-word;
}

.downloading .state,
.verifying .state,
.extracting .state {
  color: var(--accent);
}

.done .state {
  color: var(--ok);
}

.state.bad,
.bad {
  color: var(--danger);
}

.size {
  white-space: nowrap;
}

.live {
  display: grid;
  justify-items: end;
  font-size: 12.5px;
  min-width: 6.5em;
}

.live strong {
  font-size: 14px;
}

.actions {
  display: flex;
  justify-content: flex-end;
  gap: 2px;
  min-width: 62px;
}

.details {
  margin: 10px 0 0 30px;
  padding-top: 10px;
  border-top: 1px solid var(--border);
}

.options {
  display: flex;
  flex-wrap: wrap;
  gap: 10px 22px;
  align-items: center;
}

.options label,
.order {
  display: flex;
  gap: 8px;
  align-items: center;
}

.limit {
  width: 7em;
}

.dest {
  margin: 10px 0;
  font-size: 12.5px;
  overflow-wrap: anywhere;
}

.dest span {
  color: var(--fg);
}

.files {
  list-style: none;
  margin: 0;
  padding: 0;
}

.files > li {
  padding: 3px 0;
  border-top: 1px solid var(--border);
}

.files > li:first-child {
  border-top: 0;
}

.frow {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto 6.5em 7em 64px;
  gap: 10px;
  align-items: center;
  min-height: 34px;
}

.fname {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.fsize {
  font-size: 12.5px;
}

.fspeed {
  text-align: right;
}

.fstatus {
  font-size: 12.5px;
}

.fbtns {
  display: flex;
  justify-content: flex-end;
}

.icon-btn.on {
  background: var(--accent-soft);
  color: var(--accent);
}

@media (max-width: 760px) {
  .live {
    display: none;
  }

  .frow {
    grid-template-columns: minmax(0, 1fr) auto;
  }

  .fsize,
  .fspeed {
    display: none;
  }

  .details {
    margin-left: 0;
  }
}
</style>
