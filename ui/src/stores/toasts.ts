import { defineStore } from "pinia";
import { ref } from "vue";

export interface Toast {
  id: number;
  level: "info" | "error";
  message: string;
}

export const useToastStore = defineStore("toasts", () => {
  const toasts = ref<Toast[]>([]);
  let next = 1;

  function dismiss(id: number) {
    toasts.value = toasts.value.filter((toast) => toast.id !== id);
  }

  function push(level: Toast["level"], message: string) {
    const id = next++;
    toasts.value = [...toasts.value.slice(-3), { id, level, message }];
    setTimeout(() => dismiss(id), level === "error" ? 10000 : 4000);
  }

  return {
    toasts,
    dismiss,
    info: (message: string) => push("info", message),
    error: (message: string) => push("error", message),
  };
});
