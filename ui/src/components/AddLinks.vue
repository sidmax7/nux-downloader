<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { api } from "../api";
import type { DraftDto, PriorityDto } from "../bindings";
import { formatBytes, splitLinks } from "../format";
import { useSettingsStore } from "../stores/settings";
import { useToastStore } from "../stores/toasts";
import ModalDialog from "./ModalDialog.vue";

const props = defineProps<{ initialText?: string }>();
const emit = defineEmits<{ close: [] }>();

const settings = useSettingsStore();
const toasts = useToastStore();

interface Row {
  draft: DraftDto;
  name: string;
  priority: PriorityDto;
  extract: boolean;
  deleteArchives: boolean;
  include: boolean;
}

const text = ref(props.initialText ?? "");
const checking = ref(false);
const adding = ref(false);
const rows = ref<Row[]>([]);
const destination = ref("");

const links = computed(() => splitLinks(text.value));
const step = computed(() => (rows.value.length > 0 ? "review" : "paste"));
const chosen = computed(() => rows.value.filter((row) => row.include && row.draft.ok_count > 0));

onMounted(async () => {
  destination.value = await api.downloadPath();
});

async function check() {
  if (links.value.length === 0) return;
  checking.value = true;
  try {
    const drafts = await api.collect(links.value);
    rows.value = drafts.map((draft) => ({
      draft,
      name: draft.name,
      priority: "normal",
      // Archives always get the checkbox; whether it starts ticked is a setting.
      extract: draft.offers_extract && (settings.settings?.extract_by_default ?? true),
      deleteArchives: settings.settings?.delete_archives_by_default ?? false,
      include: draft.ok_count > 0,
    }));
    if (drafts.length === 0) toasts.error("No links were found in that text.");
  } catch (error) {
    toasts.error(error instanceof Error ? error.message : String(error));
  } finally {
    checking.value = false;
  }
}

async function browse() {
  const picked = await api.pickFolder();
  if (picked) destination.value = picked;
}

async function add() {
  adding.value = true;
  try {
    for (const row of chosen.value) {
      await api.enqueue({
        draft_id: row.draft.id,
        name: row.name,
        dest_dir: destination.value,
        priority: row.priority,
        extract: row.extract,
        delete_archives: row.deleteArchives,
      });
    }
    toasts.info(chosen.value.length === 1 ? "Added to the queue." : "Added to the queue.");
    emit("close");
  } catch (error) {
    toasts.error(error instanceof Error ? error.message : String(error));
  } finally {
    adding.value = false;
  }
}

async function cancel() {
  await api.discardDrafts().catch(() => undefined);
  emit("close");
}

function back() {
  rows.value = [];
}

function totalSize(row: Row): string {
  const sizes = row.draft.files.map((file) => file.size);
  return sizes.every((size) => size !== null)
    ? formatBytes(sizes.reduce<number>((sum, size) => sum + (size ?? 0), 0))
    : "size unknown";
}
</script>

<template>
  <ModalDialog title="Add links" wide @close="cancel">
    <section v-if="step === 'paste'">
      <label for="links-text">Paste one or more links (one per line)</label>
      <textarea
        id="links-text"
        v-model="text"
        data-autofocus
        class="field links"
        rows="8"
        spellcheck="false"
        placeholder="https://example.com/file.zip"
      />
      <p class="muted">
        {{ links.length }} link{{ links.length === 1 ? "" : "s" }}. Multi-part archives
        (<code>.part1.rar</code>, <code>.7z.001</code>…) are grouped into one package.
      </p>
    </section>

    <section v-else class="review">
      <div class="dest">
        <label for="dest">Save to</label>
        <input id="dest" v-model="destination" class="field" />
        <button class="btn" type="button" @click="browse">Browse…</button>
      </div>

      <article v-for="row in rows" :key="row.draft.id" class="draft" :class="{ off: !row.include }">
        <div class="head">
          <input
            v-model="row.include"
            type="checkbox"
            :aria-label="`Add ${row.name}`"
            :disabled="row.draft.ok_count === 0"
          />
          <input v-model="row.name" class="field name" :aria-label="`Package name for ${row.draft.name}`" />
          <select v-model="row.priority" class="field" :aria-label="`Priority for ${row.name}`">
            <option value="high">High</option>
            <option value="normal">Normal</option>
            <option value="low">Low</option>
          </select>
        </div>
        <p class="muted num">
          {{ row.draft.files.length }} file{{ row.draft.files.length === 1 ? "" : "s" }} ·
          {{ totalSize(row) }}
        </p>
        <p v-if="row.draft.missing_volumes.length" class="warn" role="alert">
          Missing volume{{ row.draft.missing_volumes.length === 1 ? "" : "s" }}:
          {{ row.draft.missing_volumes.join(", ") }}
        </p>
        <div v-if="row.draft.offers_extract" class="post">
          <label>
            <input v-model="row.extract" type="checkbox" />
            Extract after download
          </label>
          <label :class="{ muted: !row.extract }">
            <input v-model="row.deleteArchives" type="checkbox" :disabled="!row.extract" />
            Delete archive after extraction
          </label>
        </div>
        <ul class="files">
          <li v-for="file in row.draft.files" :key="file.source">
            <span class="fname">{{ file.name ?? file.source }}</span>
            <span v-if="file.error" class="err">{{ file.error }}</span>
            <span v-else class="muted num">{{ file.size === null ? "?" : formatBytes(file.size) }}</span>
          </li>
        </ul>
      </article>
    </section>

    <template #footer>
      <button class="btn" type="button" @click="cancel">Cancel</button>
      <template v-if="step === 'paste'">
        <button
          class="btn primary"
          type="button"
          :disabled="links.length === 0 || checking"
          @click="check"
        >
          {{ checking ? "Checking…" : "Check links" }}
        </button>
      </template>
      <template v-else>
        <button class="btn" type="button" @click="back">Back</button>
        <button
          class="btn primary"
          type="button"
          :disabled="chosen.length === 0 || adding"
          @click="add"
        >
          {{ adding ? "Adding…" : `Add ${chosen.length} to queue` }}
        </button>
      </template>
    </template>
  </ModalDialog>
</template>

<style scoped>
label {
  display: block;
  margin-bottom: 4px;
}

.links {
  width: 100%;
  font-family: ui-monospace, monospace;
  resize: vertical;
}

.dest {
  display: grid;
  grid-template-columns: auto 1fr auto;
  gap: 8px;
  align-items: center;
  margin-bottom: 12px;
}

.dest label {
  margin: 0;
}

.draft {
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 10px 12px;
  margin-bottom: 10px;
}

.draft.off {
  opacity: 0.6;
}

.head {
  display: grid;
  grid-template-columns: auto 1fr auto;
  gap: 8px;
  align-items: center;
}

.post {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 20px;
  margin: 6px 0;
}

.post label {
  margin: 0;
}

.files {
  list-style: none;
  margin: 6px 0 0;
  padding: 0;
  max-height: 140px;
  overflow: auto;
  font-size: 13px;
}

.files li {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  padding: 2px 0;
}

.fname {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.err {
  color: var(--danger);
  text-align: right;
}

.warn {
  color: var(--warn);
  margin: 4px 0;
}

p {
  margin: 6px 0;
}
</style>
