<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import UiIcon from "./UiIcon.vue";

const props = defineProps<{ title: string; wide?: boolean }>();
const emit = defineEmits<{ close: [] }>();

const dialog = ref<HTMLElement | null>(null);
let opener: Element | null = null;

const FOCUSABLE =
  'button:not(:disabled), [href], input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])';

function onKey(event: KeyboardEvent) {
  if (event.key === "Escape") {
    event.stopPropagation();
    emit("close");
    return;
  }
  if (event.key !== "Tab" || !dialog.value) return;
  const items = [...dialog.value.querySelectorAll<HTMLElement>(FOCUSABLE)];
  if (items.length === 0) return;
  const first = items[0];
  const last = items[items.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

onMounted(async () => {
  opener = document.activeElement;
  await nextTick();
  const target =
    dialog.value?.querySelector<HTMLElement>("[data-autofocus]") ??
    dialog.value?.querySelector<HTMLElement>(FOCUSABLE);
  target?.focus();
});

onBeforeUnmount(() => {
  if (opener instanceof HTMLElement) opener.focus();
});
</script>

<template>
  <div class="backdrop" @mousedown.self="emit('close')">
    <div
      ref="dialog"
      class="dialog"
      :class="{ wide: props.wide }"
      role="dialog"
      aria-modal="true"
      :aria-label="props.title"
      @keydown="onKey"
    >
      <header>
        <h2>{{ props.title }}</h2>
        <button class="icon-btn" type="button" aria-label="Close" @click="emit('close')">
          <UiIcon name="close" />
        </button>
      </header>
      <div class="body"><slot /></div>
      <footer v-if="$slots.footer"><slot name="footer" /></footer>
    </div>
  </div>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  background: rgb(8 10 16 / 55%);
  animation: fade 0.15s ease-out;
  display: grid;
  place-items: center;
  padding: 16px;
  z-index: 10;
}

.dialog {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 12px;
  box-shadow: var(--shadow);
  animation: rise 0.18s cubic-bezier(0.16, 1, 0.3, 1);
  width: min(560px, 100%);
  max-height: calc(100vh - 32px);
  display: flex;
  flex-direction: column;
}

.dialog.wide {
  width: min(820px, 100%);
}

header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 12px 10px 18px;
  border-bottom: 1px solid var(--border);
}

h2 {
  margin: 0;
  font-size: 16px;
}

.body {
  padding: 16px;
  overflow: auto;
}

footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 12px 16px;
  border-top: 1px solid var(--border);
}

@keyframes fade {
  from {
    opacity: 0;
  }
}

@keyframes rise {
  from {
    opacity: 0;
    transform: translateY(8px);
  }
}
</style>
