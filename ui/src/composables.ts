import { computed } from "vue";
import { linkColor } from "./colors";
import { useEngineStore } from "./stores/engine";

/** Link id → CSS colour, for the block grid and the links panel. */
export function useLinkColors() {
  const engine = useEngineStore();
  return computed(() => {
    const colors = new Map<number, string>();
    for (const link of engine.links) colors.set(link.id, linkColor(link.name, link.color));
    return colors;
  });
}
