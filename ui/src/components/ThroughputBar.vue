<script setup lang="ts">
// The product's one signature: every network link has a colour, and the
// total speed is drawn as the colours' shares.
import { computed } from "vue";
import type { LinkDto } from "../bindings";
import { linkColor } from "../colors";
import { formatSpeed } from "../format";

const props = defineProps<{ links: LinkDto[]; legend?: boolean }>();

const live = computed(() =>
  props.links.filter((link) => link.enabled && link.active > 0 && link.speed > 0),
);
const total = computed(() => live.value.reduce((sum, link) => sum + link.speed, 0));
</script>

<template>
  <div class="throughput">
    <div class="track" aria-hidden="true">
      <div
        v-for="link in live"
        :key="link.name"
        class="seg"
        :style="{ flexGrow: link.speed, background: linkColor(link.name, link.color) }"
      />
    </div>
    <ul v-if="props.legend && props.links.length" class="legend">
      <li v-for="link in props.links" :key="link.name" :class="{ off: !link.enabled }">
        <span class="dot" :style="{ background: linkColor(link.name, link.color) }" />
        <span class="n">{{ link.label }}</span>
        <span class="num muted">{{ link.enabled ? formatSpeed(link.active > 0 ? link.speed : 0) : "off" }}</span>
      </li>
    </ul>
    <span class="visually-hidden">Total {{ formatSpeed(total) }}</span>
  </div>
</template>

<style scoped>
.track {
  display: flex;
  gap: 2px;
  height: 6px;
  border-radius: 3px;
  background: var(--track);
  overflow: hidden;
}

.seg {
  min-width: 3px;
  transition: flex-grow 0.25s ease-out;
}

.legend {
  list-style: none;
  margin: 8px 0 0;
  padding: 0;
  display: grid;
  gap: 4px;
  font-size: 12px;
}

.legend li {
  display: flex;
  align-items: center;
  gap: 6px;
}

.legend .n {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
  min-width: 0;
}

.legend .off {
  opacity: 0.55;
}

.dot {
  width: 8px;
  height: 8px;
  border-radius: 2px;
  flex: none;
}
</style>
