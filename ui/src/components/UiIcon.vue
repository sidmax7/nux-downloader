<script setup lang="ts">
// Decorative by
// default (the button carries the label); pass `title` to make it speak.
import { computed } from "vue";
import { type IconName, PATHS } from "../icons";



const props = defineProps<{ name: IconName; size?: number; title?: string }>();
const d = computed(() => PATHS[props.name]);
</script>

<template>
  <svg
    class="icon"
    :width="props.size ?? 16"
    :height="props.size ?? 16"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.75"
    stroke-linecap="round"
    stroke-linejoin="round"
    :role="props.title ? 'img' : undefined"
    :aria-label="props.title"
    :aria-hidden="props.title ? undefined : 'true'"
  >
    <path :d="d" />
  </svg>
</template>

<style scoped>
.icon {
  flex: none;
  display: inline-block;
  vertical-align: -0.2em;
}
</style>
