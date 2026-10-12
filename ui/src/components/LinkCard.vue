<script setup lang="ts">
import { computed, ref, watch } from "vue";
import type { LinkDto } from "../bindings";
import { linkColor } from "../colors";
import { formatBytes, formatSpeed, parseRate, rateText } from "../format";
import { useSettingsStore } from "../stores/settings";
import { useToastStore } from "../stores/toasts";

const props = defineProps<{ link: LinkDto; peak: number }>();

const settings = useSettingsStore();
const toasts = useToastStore();

const label = ref(props.link.label);
const limit = ref(rateText(props.link.limit));
const color = computed(() => linkColor(props.link.name, props.link.color));
const share = computed(() =>
  props.peak > 0 ? Math.min(100, Math.round((props.link.speed / props.peak) * 100)) : 0,
);

watch(() => props.link.label, (value) => (label.value = value));
watch(() => props.link.limit, (value) => (limit.value = rateText(value)));

async function saveLabel() {
  const trimmed = label.value.trim();
  await settings.setLinkPref(props.link.name, { label: trimmed || null });
}

async function saveLimit() {
  const parsed = parseRate(limit.value);
  if (parsed === undefined) {
    toasts.error("A speed limit looks like 500K, 5M or 1.5G. Leave it empty for no limit.");
    return;
  }
  await settings.setLinkPref(props.link.name, { limit: parsed });
}
</script>

<template>
  <li class="card" :class="{ off: !link.enabled }">
    <div class="top">
      <input
        type="color"
        class="swatch"
        :value="color"
        :aria-label="`Colour of ${link.label}`"
        @change="settings.setLinkPref(link.name, { color: ($event.target as HTMLInputElement).value })"
      />
      <input
        v-model="label"
        class="field name"
        :aria-label="`Name of ${link.name}`"
        @keydown.enter="($event.target as HTMLInputElement).blur()"
        @blur="saveLabel"
      />
      <label class="switch">
        <input
          type="checkbox"
          :checked="link.enabled"
          @change="settings.setLinkPref(link.name, { enabled: ($event.target as HTMLInputElement).checked })"
        />
        Use this link
      </label>
    </div>

    <p class="muted meta">
      {{ link.name }} · {{ link.kind }}<template v-if="link.address"> · {{ link.address }}</template>
    </p>
    <p v-if="link.degraded" class="warn" role="alert">
      Connections keep failing on this link; it is paused and will be retried shortly.
    </p>

    <div class="speed">
      <div class="meter" aria-hidden="true">
        <div class="meter-fill" :style="{ width: `${share}%`, background: color }" />
      </div>
      <strong class="num">{{ formatSpeed(link.speed) }}</strong>
    </div>
    <p class="muted num stats">
      {{ link.active }} connection{{ link.active === 1 ? "" : "s" }} · {{ formatBytes(link.bytes) }} this session
    </p>

    <label class="limit">
      Speed limit
      <input
        v-model="limit"
        class="field"
        placeholder="none"
        @keydown.enter="($event.target as HTMLInputElement).blur()"
        @blur="saveLimit"
      />
    </label>
  </li>
</template>

<style scoped>
.card {
  list-style: none;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px;
}

.card.off {
  opacity: 0.65;
}

.top {
  display: grid;
  grid-template-columns: auto 1fr auto;
  gap: 8px;
  align-items: center;
}

.swatch {
  width: 28px;
  height: 28px;
  padding: 0;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: none;
}

.switch {
  display: flex;
  gap: 6px;
  align-items: center;
}

.meta,
.stats {
  margin: 6px 0 0;
  font-size: 12px;
}

.warn {
  color: var(--warn);
  margin: 6px 0 0;
}

.speed {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 10px;
  align-items: center;
  margin-top: 10px;
}

.meter {
  height: 8px;
  background: var(--surface-2);
  border-radius: 4px;
  overflow: hidden;
}

.meter-fill {
  height: 100%;
  transition: width 0.25s linear;
}

.limit {
  display: flex;
  gap: 8px;
  align-items: center;
  margin-top: 8px;
}

.limit .field {
  width: 8em;
}
</style>
