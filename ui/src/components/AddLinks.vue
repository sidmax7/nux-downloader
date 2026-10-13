<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { api } from "../api";
import type { DraftDto, DraftFileDto, PriorityDto } from "../bindings";
import { type FileKind, KIND_LABEL, KIND_ORDER, fileKind } from "../fileKinds";
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
  /** Ticked files, by source. */
  picked: Record<string, boolean>;
  /** Whether its file list is shown. */
  open: boolean;
}

/** Up to this many files in all, every package starts open. */
const OPEN_LIMIT = 12;

// The two scan options are a per-person habit, so they are remembered in
// this browser only (a missing or blocked storage just means defaults).
const SCAN_KEY = "nuxd.add.scan";
function loadScan(): { pages: boolean; deep: boolean } {
  try {
    const saved = JSON.parse(localStorage.getItem(SCAN_KEY) ?? "null");
    if (saved && typeof saved.pages === "boolean" && typeof saved.deep === "boolean") return saved;
  } catch {
    // fall through to the defaults
  }
  return { pages: true, deep: false };
}

const text = ref(props.initialText ?? "");
const checking = ref(false);
const adding = ref(false);
const rows = ref<Row[]>([]);
const destination = ref("");
const scan = ref(loadScan());

watch(
  scan,
  (value) => {
    try {
      localStorage.setItem(SCAN_KEY, JSON.stringify(value));
    } catch {
      // not saved; fine
    }
  },
  { deep: true },
);

const links = computed(() => splitLinks(text.value));
const step = computed(() => (rows.value.length > 0 ? "review" : "paste"));
const usable = (file: DraftFileDto) => file.error === null;
const pickedFiles = (row: Row) => row.draft.files.filter((f) => usable(f) && row.picked[f.source]);
const chosen = computed(() => rows.value.filter((row) => pickedFiles(row).length > 0));
const chosenFiles = computed(() => chosen.value.reduce((n, row) => n + pickedFiles(row).length, 0));
const totalFiles = computed(() =>
  rows.value.reduce((n, row) => n + row.draft.files.filter(usable).length, 0),
);
const chosenSize = computed(() => sizeOf(rows.value.flatMap(pickedFiles)));
const allOpen = computed(() => rows.value.every((row) => row.open));

function setAllOpen(open: boolean) {
  for (const row of rows.value) row.open = open;
}

onMounted(async () => {
  destination.value = await api.downloadPath();
});

interface Kind {
  kind: FileKind;
  label: string;
  total: number;
  picked: number;
}

/** What the batch holds, by kind, and how much of each is ticked. */
const kinds = computed<Kind[]>(() => {
  const counts = new Map<FileKind, { total: number; picked: number }>();
  for (const row of rows.value) {
    for (const file of row.draft.files) {
      if (!usable(file)) continue;
      const kind = fileKind(file.name ?? file.source);
      const entry = counts.get(kind) ?? { total: 0, picked: 0 };
      entry.total += 1;
      if (row.picked[file.source]) entry.picked += 1;
      counts.set(kind, entry);
    }
  }
  return KIND_ORDER.filter((kind) => counts.has(kind)).map((kind) => ({
    kind,
    label: KIND_LABEL[kind],
    ...(counts.get(kind) as { total: number; picked: number }),
  }));
});

/** Ticks every file of `kind` — or unticks them all when they already are. */
function toggleKind(entry: Kind) {
  const on = entry.picked < entry.total;
  for (const row of rows.value) {
    for (const file of row.draft.files) {
      if (usable(file) && fileKind(file.name ?? file.source) === entry.kind) {
        row.picked[file.source] = on;
      }
    }
  }
}

/** Ticks only `kind`, for "just the videos". */
function only(entry: Kind) {
  for (const row of rows.value) {
    for (const file of row.draft.files) {
      if (usable(file)) row.picked[file.source] = fileKind(file.name ?? file.source) === entry.kind;
    }
  }
}

function pressed(entry: Kind): "true" | "false" | "mixed" {
  if (entry.picked === 0) return "false";
  return entry.picked === entry.total ? "true" : "mixed";
}

async function check() {
  if (links.value.length === 0) return;
  checking.value = true;
  try {
    const drafts = await api.collect(links.value, {
      scan_pages: scan.value.pages,
      deep: scan.value.pages && scan.value.deep,
    });
    const files = drafts.reduce((n, d) => n + d.files.length, 0);
    const open = drafts.length === 1 || files <= OPEN_LIMIT;
    rows.value = drafts.map((draft) => ({
      draft,
      name: draft.name,
      priority: "normal",
      // Archives always get the checkbox; whether it starts ticked is a setting.
      extract: draft.offers_extract && (settings.settings?.extract_by_default ?? true),
      deleteArchives: settings.settings?.delete_archives_by_default ?? false,
      picked: Object.fromEntries(draft.files.map((f) => [f.source, usable(f)])),
      open,
    }));
    if (drafts.length === 0) {
      toasts.error(
        scan.value.pages
          ? "No files were found. Try “Also look in pages they link to”, or paste direct links."
          : "No links were found in that text.",
      );
    }
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
      const files = pickedFiles(row);
      const all = files.length === row.draft.files.length;
      await api.enqueue({
        draft_id: row.draft.id,
        name: row.name,
        dest_dir: destination.value,
        priority: row.priority,
        extract: row.extract,
        delete_archives: row.deleteArchives,
        files: all ? null : files.map((f) => f.source),
      });
    }
    toasts.info("Added to the queue.");
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

function allPicked(row: Row): boolean {
  const files = row.draft.files.filter(usable);
  return files.length > 0 && files.every((f) => row.picked[f.source]);
}

function somePicked(row: Row): boolean {
  const n = pickedFiles(row).length;
  return n > 0 && n < row.draft.files.filter(usable).length;
}

function pickAll(row: Row, on: boolean) {
  for (const f of row.draft.files) if (usable(f)) row.picked[f.source] = on;
}

function sizeOf(files: DraftFileDto[]): string {
  if (files.length === 0) return "nothing selected";
  return files.every((f) => f.size !== null)
    ? formatBytes(files.reduce<number>((sum, f) => sum + (f.size ?? 0), 0))
    : "size unknown";
}

function pickedSize(row: Row): string {
  return sizeOf(pickedFiles(row));
}

interface SavesTo {
  /** The last folders of the path, for the row. */
  short: string;
  full: string;
  /** The parts go in the folder's `Archives`. */
  archives: boolean;
}

/**
 * Where the row's files will land, by the engine's rule (Decision 37):
 * a folder of its own for an archive or several files, unless Settings
 * turned package folders off. The engine may add " (1)" to the name if a
 * folder of that name is already in use.
 */
function savesTo(row: Row): SavesTo {
  const base = destination.value.replace(/[\\/]+$/, "");
  const baseName = base.split(/[\\/]/).pop() || base;
  const own =
    (settings.settings?.package_folders ?? true) &&
    (row.draft.offers_extract || pickedFiles(row).length > 1);
  if (!own) return { short: `${baseName}/`, full: base, archives: false };
  const name = row.name.trim() || row.draft.name;
  return {
    short: `${baseName}/${name}/`,
    full: `${base}/${name}`,
    archives: row.draft.offers_extract && row.extract,
  };
}

/** "Found on example.com/album/x" when every file came from one page. */
function foundOn(row: Row): string | null {
  const pages = new Set(row.draft.files.map((f) => f.found_on));
  const [page] = [...pages];
  if (pages.size !== 1 || !page) return null;
  try {
    const url = new URL(page);
    return `${url.host}${url.pathname === "/" ? "" : url.pathname}`;
  } catch {
    return page;
  }
}

function partlyPickedArchive(row: Row): boolean {
  return row.draft.offers_extract && row.draft.files.length > 1 && somePicked(row);
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

      <fieldset class="options">
        <legend class="sr-only">Web pages</legend>
        <label class="option">
          <input v-model="scan.pages" type="checkbox" />
          <span>
            <strong>Find files on web pages</strong>
            <small>A link to a page (an album, a gallery, a folder listing) is replaced by the
              files it links to or shows.</small>
          </span>
        </label>
        <label class="option nested" :class="{ off: !scan.pages }">
          <input v-model="scan.deep" type="checkbox" :disabled="!scan.pages" />
          <span>
            <strong>Also look in pages they link to</strong>
            <small>One level deeper, on the same site: for albums where every file has its own
              page. Slower.</small>
          </span>
        </label>
      </fieldset>
    </section>

    <section v-else class="review">
      <div class="dest">
        <label for="dest">Save to</label>
        <input id="dest" v-model="destination" class="field" />
        <button class="btn" type="button" @click="browse">Browse…</button>
      </div>

      <div v-if="kinds.length > 1" class="kinds" role="group" aria-label="Pick files by type">
        <span class="kinds-label">Pick by type</span>
        <span v-for="entry in kinds" :key="entry.kind" class="kind">
          <button
            type="button"
            class="chip"
            :class="`is-${pressed(entry)}`"
            :aria-pressed="pressed(entry)"
            :title="`${entry.picked} of ${entry.total} ticked. Click to ${entry.picked < entry.total ? 'tick' : 'untick'} all`"
            @click="toggleKind(entry)"
          >
            <span class="tick" aria-hidden="true" />
            {{ entry.label }}
            <span class="count num">{{ entry.total }}</span>
          </button>
          <button
            type="button"
            class="only"
            :aria-label="`Only ${entry.label.toLowerCase()}`"
            @click="only(entry)"
          >
            only
          </button>
        </span>
      </div>

      <div class="summary">
        <span class="muted num">
          {{ rows.length }} package{{ rows.length === 1 ? "" : "s" }} · {{ chosenFiles }} of
          {{ totalFiles }} file{{ totalFiles === 1 ? "" : "s" }} · {{ chosenSize }}
        </span>
        <button v-if="rows.length > 1" type="button" class="textbtn" @click="setAllOpen(!allOpen)">
          {{ allOpen ? "Collapse all" : "Expand all" }}
        </button>
      </div>

      <article
        v-for="row in rows"
        :key="row.draft.id"
        class="draft"
        :class="{ off: pickedFiles(row).length === 0, open: row.open }"
      >
        <div class="head">
          <button
            type="button"
            class="disclose"
            :aria-expanded="row.open"
            :aria-controls="`files-${row.draft.id}`"
            :aria-label="`${row.open ? 'Hide' : 'Show'} the files of ${row.name}`"
            @click="row.open = !row.open"
          >
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" /></svg>
          </button>
          <input
            type="checkbox"
            :checked="allPicked(row)"
            :indeterminate="somePicked(row)"
            :aria-label="`All files of ${row.name}`"
            :disabled="row.draft.ok_count === 0"
            @change="pickAll(row, ($event.target as HTMLInputElement).checked)"
          />
          <input v-model="row.name" class="field name" :aria-label="`Package name for ${row.draft.name}`" />
          <select v-model="row.priority" class="field" :aria-label="`Priority for ${row.name}`">
            <option value="high">High</option>
            <option value="normal">Normal</option>
            <option value="low">Low</option>
          </select>
        </div>
        <div class="body">
          <p class="muted num meta">
            {{ pickedFiles(row).length }} of {{ row.draft.files.length }}
            file{{ row.draft.files.length === 1 ? "" : "s" }} · {{ pickedSize(row) }}
            <template v-if="foundOn(row)"> · found on <span class="page">{{ foundOn(row) }}</span></template>
          </p>
          <p class="muted saves">
            Saves to <span class="path" :title="savesTo(row).full">{{ savesTo(row).short }}</span>
            <template v-if="savesTo(row).archives"> · parts in <span class="path">Archives/</span></template>
          </p>
          <p v-if="row.draft.missing_volumes.length" class="warn" role="alert">
            Missing volume{{ row.draft.missing_volumes.length === 1 ? "" : "s" }}:
            {{ row.draft.missing_volumes.join(", ") }}
          </p>
          <p v-if="partlyPickedArchive(row)" class="warn" role="alert">
            Some parts of this archive are unticked; it can’t be extracted without them.
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
        </div>
        <div
          :id="`files-${row.draft.id}`"
          class="files-wrap"
          :class="{ open: row.open }"
          :inert="!row.open || undefined"
        >
          <div class="files-inner">
            <ul class="files">
              <li v-for="file in row.draft.files" :key="file.source" :class="{ off: !row.picked[file.source] }">
                <label class="file">
                  <input
                    v-model="row.picked[file.source]"
                    type="checkbox"
                    :disabled="!usable(file)"
                    :aria-label="`Add ${file.name ?? file.source}`"
                  />
                  <span class="fname" :title="file.source">{{ file.name ?? file.source }}</span>
                </label>
                <span v-if="file.error" class="err">{{ file.error }}</span>
                <span v-else class="muted num">{{ file.size === null ? "?" : formatBytes(file.size) }}</span>
              </li>
            </ul>
          </div>
        </div>
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
          {{ checking ? (scan.pages ? "Checking links and pages…" : "Checking…") : "Check links" }}
        </button>
      </template>
      <template v-else>
        <button class="btn" type="button" @click="back">Back</button>
        <button
          class="btn primary"
          type="button"
          :disabled="chosenFiles === 0 || adding"
          @click="add"
        >
          {{ adding ? "Adding…" : `Add ${chosenFiles} file${chosenFiles === 1 ? "" : "s"}` }}
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

.options {
  margin: 10px 0 0;
  padding: 0;
  border: 0;
  display: grid;
  gap: 4px;
}

.option {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  margin: 0;
  padding: 8px 10px;
  border-radius: 8px;
  cursor: pointer;
}

.option:hover {
  background: var(--surface-2);
}

.option input {
  margin-top: 1px;
}

.option span {
  display: grid;
  gap: 1px;
}

.option small {
  color: var(--muted);
  font-size: 12.5px;
}

.option.nested {
  margin-left: 28px;
}

.option.off {
  opacity: 0.55;
  cursor: default;
}

.option.off:hover {
  background: none;
}

.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
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
  padding: 8px 12px 8px 8px;
  margin-bottom: 8px;
}

.draft.off {
  opacity: 0.6;
}

.head {
  display: grid;
  grid-template-columns: 24px auto 1fr auto;
  gap: 8px;
  align-items: center;
}

.disclose {
  display: grid;
  place-items: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: 0;
  border-radius: 6px;
  background: none;
  color: var(--muted);
  cursor: pointer;
}

.disclose:hover {
  background: var(--surface-2);
  color: var(--fg);
}

.disclose:focus-visible,
.textbtn:focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 2px;
}

.disclose svg {
  width: 14px;
  height: 14px;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.8;
  stroke-linecap: round;
  stroke-linejoin: round;
  transition: transform 160ms ease;
}

.draft.open .disclose svg {
  transform: rotate(90deg);
}

/* Lines under the name line up with the checkbox, past the toggle. */
.body {
  padding-left: 32px;
  margin-top: 4px;
}

.body p {
  margin: 2px 0;
}

.body .post {
  margin: 4px 0 2px;
}

.meta,
.saves {
  font-size: 12.5px;
}

.path {
  color: var(--fg);
  overflow-wrap: anywhere;
}

.files-wrap {
  display: grid;
  grid-template-rows: 0fr;
  transition: grid-template-rows 180ms ease;
}

.files-wrap.open {
  grid-template-rows: 1fr;
}

.files-inner {
  min-height: 0;
  overflow: hidden;
  padding-left: 32px;
}

.summary {
  display: flex;
  justify-content: space-between;
  align-items: baseline;
  gap: 12px;
  margin: 0 0 8px;
}

.textbtn {
  padding: 2px 4px;
  border: 0;
  border-radius: 6px;
  background: none;
  color: var(--accent);
  font: inherit;
  font-size: 13px;
  cursor: pointer;
}

.textbtn:hover {
  text-decoration: underline;
}

.meta .page {
  overflow-wrap: anywhere;
}

.kinds {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px 10px;
  margin: 0 0 12px;
}

.kinds-label {
  color: var(--muted);
  font-size: 12.5px;
}

.kind {
  display: inline-flex;
  align-items: center;
  gap: 2px;
}

.chip {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 4px 10px 4px 8px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: var(--surface);
  color: var(--fg);
  font: inherit;
  cursor: pointer;
  transition:
    background-color 120ms ease,
    border-color 120ms ease;
}

.chip:hover {
  border-color: var(--accent);
}

.chip:focus-visible,
.only:focus-visible {
  outline: 2px solid var(--focus);
  outline-offset: 2px;
}

.chip.is-true,
.chip.is-mixed {
  border-color: var(--accent);
  background: var(--accent-soft);
}

.chip .tick {
  width: 14px;
  height: 14px;
  border: 1.5px solid var(--check-border);
  border-radius: 4px;
  background: var(--surface) no-repeat center / 10px 10px;
}

.chip.is-true .tick,
.chip.is-mixed .tick {
  border-color: var(--accent);
  background-color: var(--accent);
}

.chip.is-true .tick {
  background-image: var(--check-mark);
}

.chip.is-mixed .tick {
  background-image: var(--check-mixed);
}

.chip .count {
  color: var(--muted);
  font-size: 12.5px;
}

.only {
  padding: 2px 6px;
  border: 0;
  border-radius: 6px;
  background: none;
  color: var(--muted);
  font: inherit;
  font-size: 12px;
  cursor: pointer;
  opacity: 0;
  transition: opacity 120ms ease;
}

.kind:hover .only,
.only:focus-visible {
  opacity: 1;
}

.only:hover {
  color: var(--accent);
  text-decoration: underline;
}

@media (hover: none) {
  .only {
    opacity: 1;
  }
}

@media (prefers-reduced-motion: reduce) {
  .chip,
  .only,
  .disclose svg,
  .files-wrap {
    transition: none;
  }
}

.post {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 20px;
  margin: 6px 0;
}

.post label {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 0;
}

.files {
  list-style: none;
  margin: 4px 0 2px;
  padding: 0;
  max-height: 240px;
  overflow: auto;
  font-size: 13px;
}

.files li {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  padding: 3px 4px;
  border-radius: 6px;
}

.files li:hover {
  background: var(--surface-2);
}

.files li.off .fname {
  color: var(--muted);
}

.file {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  margin: 0;
  cursor: pointer;
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
