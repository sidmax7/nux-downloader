<script setup lang="ts">
import { useToastStore } from "../stores/toasts";
import UiIcon from "./UiIcon.vue";

const toasts = useToastStore();
</script>

<template>
  <div class="host" role="status" aria-live="polite">
    <div v-for="toast in toasts.toasts" :key="toast.id" class="toast" :class="toast.level">
      <span>{{ toast.message }}</span>
      <button class="icon-btn" type="button" aria-label="Dismiss" @click="toasts.dismiss(toast.id)">
        <UiIcon name="close" :size="14" />
      </button>
    </div>
  </div>
</template>

<style scoped>
.host {
  position: fixed;
  right: 16px;
  bottom: 16px;
  display: grid;
  gap: 8px;
  width: min(380px, calc(100vw - 32px));
  z-index: 20;
}

.toast {
  display: flex;
  justify-content: space-between;
  gap: 8px;
  align-items: center;
  padding: 8px 8px 8px 14px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 9px;
  box-shadow: var(--shadow);
  overflow-wrap: anywhere;
}

.toast.error {
  border-color: var(--danger);
}
</style>
