<script setup lang="ts">
import { computed, ref } from "vue";
import type { PackageDto } from "../bindings";
import { useEngineStore } from "../stores/engine";
import UiIcon from "./UiIcon.vue";
import PackageRow from "./PackageRow.vue";

const emit = defineEmits<{ add: [] }>();

const engine = useEngineStore();

type Filter = "all" | "active" | "paused" | "done" | "failed";

const filter = ref<Filter>("all");

const matches: Record<Filter, (p: PackageDto) => boolean> = {
  all: () => true,
  active: (p) => ["downloading", "queued", "verifying", "extracting"].includes(p.state),
  paused: (p) => p.state === "paused",
  done: (p) => p.state === "done",
  failed: (p) => p.state === "failed",
};

const LABELS: Record<Filter, string> = {
  all: "All",
  active: "Active",
  paused: "Paused",
  done: "Done",
  failed: "Failed",
};

const counts = computed(() => {
  const result = {} as Record<Filter, number>;
  for (const key of Object.keys(matches) as Filter[]) {
    result[key] = engine.packages.filter(matches[key]).length;
  }
  return result;
});

// A filter with nothing in it is hidden, except the one in use.
const filters = computed(() =>
  (Object.keys(matches) as Filter[]).filter(
    (key) => key === "all" || key === filter.value || counts.value[key] > 0,
  ),
);

const shown = computed(() => engine.packages.filter(matches[filter.value]));

async function clearDone() {
  const done = engine.packages.filter(matches.done);
  for (const pkg of done) await engine.remove(pkg.id, false);
}
</script>

<template>
  <section aria-label="Downloads">
    <div v-if="engine.packages.length === 0" class="empty">
      <UiIcon name="download" :size="28" />
      <h2>Nothing downloading</h2>
      <p class="muted">
        Paste a link anywhere in this window, or press <kbd>Ctrl</kbd> <kbd>N</kbd>. Several links to
        the same archive (<code>.part1.rar</code>, <code>.7z.001</code>…) become one package, with
        an option to extract it when it arrives.
      </p>
      <button class="btn primary" type="button" @click="emit('add')">
        <UiIcon name="plus" /> Add links
      </button>
    </div>

    <template v-else>
      <div class="tools">
        <div class="filters" role="group" aria-label="Filter downloads">
          <button
            v-for="key in filters"
            :key="key"
            type="button"
            class="chip"
            :class="{ on: filter === key }"
            :aria-pressed="filter === key"
            @click="filter = key"
          >
            {{ LABELS[key] }} <span class="count num">{{ counts[key] }}</span>
          </button>
        </div>
        <button v-if="counts.done > 0" class="btn small clear" type="button" @click="clearDone">
          Clear finished
        </button>
      </div>

      <ul class="list">
        <PackageRow
          v-for="(pkg, index) in shown"
          :key="pkg.id"
          :pkg="pkg"
          :index="engine.packages.indexOf(pkg)"
          :count="engine.packages.length"
          :first="index === 0"
        />
      </ul>
      <p v-if="shown.length === 0" class="muted none">Nothing here.</p>
    </template>
  </section>
</template>

<style scoped>
.empty {
  display: grid;
  justify-items: center;
  gap: 10px;
  max-width: 460px;
  margin: 64px auto;
  text-align: center;
  color: var(--muted);
}

.empty h2 {
  margin: 4px 0 0;
  font-size: 16px;
  color: var(--fg);
}

.empty p {
  margin: 0 0 6px;
}

kbd {
  font: inherit;
  font-size: 12px;
  padding: 1px 6px;
  border: 1px solid var(--border);
  border-bottom-width: 2px;
  border-radius: 5px;
  background: var(--surface);
  color: var(--fg);
}

.tools {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-bottom: 12px;
}

.filters {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}

.chip {
  display: inline-flex;
  gap: 6px;
  align-items: center;
  padding: 3px 11px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: transparent;
  color: var(--muted);
  cursor: pointer;
  transition:
    background-color 0.15s ease-out,
    color 0.15s ease-out;
}

.chip:hover {
  background: var(--surface-2);
  color: var(--fg);
}

.chip.on {
  background: var(--accent-soft);
  border-color: transparent;
  color: var(--accent);
  font-weight: 600;
}

.clear {
  white-space: nowrap;
}

.count {
  font-size: 12px;
  opacity: 0.8;
}

.list {
  margin: 0;
  padding: 0;
  display: grid;
  gap: 8px;
}

.none {
  text-align: center;
  padding: 32px 0;
}
</style>
