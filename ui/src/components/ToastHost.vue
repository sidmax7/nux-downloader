<script setup lang="ts">
import { useToastStore } from "../stores/toasts";

const toasts = useToastStore();
</script>

<template>
  <div class="host" role="status" aria-live="polite">
    <div v-for="toast in toasts.toasts" :key="toast.id" class="toast" :class="toast.level">
      <span>{{ toast.message }}</span>
      <button class="btn small" type="button" aria-label="Dismiss" @click="toasts.dismiss(toast.id)">
        ✕
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
  align-items: start;
  padding: 10px 12px;
  background: var(--surface);
  border: 1px solid var(--border);
  border-left: 4px solid var(--accent);
  border-radius: 6px;
  box-shadow: 0 4px 16px rgb(0 0 0 / 20%);
  overflow-wrap: anywhere;
}

.toast.error {
  border-left-color: var(--danger);
}
</style>
