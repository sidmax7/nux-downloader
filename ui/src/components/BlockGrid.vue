<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { api } from "../api";
import type { BlockDto } from "../bindings";
import { useLinkColors } from "../composables";
import { bucketBlocks, cellBudget, gridGeometry } from "../grid";

const props = defineProps<{ packageId: number; fileId: number; live: boolean }>();

const CELL = 10;
const GAP = 2;
const MAX_ROWS = 24;

const canvas = ref<HTMLCanvasElement | null>(null);
const blocks = ref<BlockDto[]>([]);
const width = ref(0);
const colors = useLinkColors();
let timer: ReturnType<typeof setInterval> | undefined;
let observer: ResizeObserver | undefined;

const finished = computed(() => blocks.value.filter((b) => b.done >= 100).length);
const label = computed(
  () => `Block map: ${finished.value} of ${blocks.value.length} blocks complete`,
);

async function refresh() {
  try {
    blocks.value = (await api.blocks(props.packageId, props.fileId)).blocks;
  } catch {
    // the file may have just left the queue; the next snapshot removes this grid
  }
}

function cssColor(name: string, fallback: string): string {
  const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return value || fallback;
}

function draw() {
  const el = canvas.value;
  if (!el || width.value === 0) return;
  const cells = bucketBlocks(blocks.value, cellBudget(width.value, MAX_ROWS, CELL, GAP));
  const geometry = gridGeometry(cells.length, width.value, CELL, GAP);
  const height = geometry.rows * (CELL + GAP) - GAP;
  const ratio = window.devicePixelRatio || 1;
  el.width = Math.round(width.value * ratio);
  el.height = Math.round(Math.max(height, 1) * ratio);
  el.style.height = `${Math.max(height, 1)}px`;
  const ctx = el.getContext("2d");
  if (!ctx) return;
  ctx.scale(ratio, ratio);
  const empty = cssColor("--surface-2", "#ddd");
  const border = cssColor("--border", "#bbb");
  cells.forEach((cell, index) => {
    const x = (index % geometry.columns) * (CELL + GAP);
    const y = Math.floor(index / geometry.columns) * (CELL + GAP);
    ctx.fillStyle = empty;
    ctx.fillRect(x, y, CELL, CELL);
    if (cell.done > 0) {
      const fill = Math.max(1, Math.round((CELL * cell.done) / 100));
      ctx.fillStyle = (cell.link !== null && colors.value.get(cell.link)) || cssColor("--accent", "#27c");
      ctx.fillRect(x, y + CELL - fill, CELL, fill);
    } else {
      ctx.strokeStyle = border;
      ctx.strokeRect(x + 0.5, y + 0.5, CELL - 1, CELL - 1);
    }
  });
}

function schedule() {
  clearInterval(timer);
  timer = props.live ? setInterval(refresh, 250) : undefined;
}

onMounted(async () => {
  if (canvas.value?.parentElement) {
    observer = new ResizeObserver((entries) => {
      width.value = Math.floor(entries[0]?.contentRect.width ?? 0);
    });
    observer.observe(canvas.value.parentElement);
  }
  await refresh();
  schedule();
});

onBeforeUnmount(() => {
  clearInterval(timer);
  observer?.disconnect();
});

watch(() => props.live, async () => {
  schedule();
  await refresh();
});
watch([blocks, width, colors], draw, { flush: "post" });
</script>

<template>
  <div class="grid">
    <canvas ref="canvas" role="img" :aria-label="label" />
    <p v-if="blocks.length === 0" class="muted">No block map yet.</p>
  </div>
</template>

<style scoped>
.grid {
  width: 100%;
}

canvas {
  display: block;
  width: 100%;
}

p {
  margin: 0;
  font-size: 12px;
}
</style>
