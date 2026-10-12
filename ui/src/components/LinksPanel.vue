<script setup lang="ts">
import { computed } from "vue";
import { formatSpeed } from "../format";
import { useEngineStore } from "../stores/engine";
import LinkCard from "./LinkCard.vue";
import ThroughputBar from "./ThroughputBar.vue";

const engine = useEngineStore();
const enabledCount = computed(() => engine.links.filter((link) => link.enabled).length);
const peak = computed(() => Math.max(0, ...engine.links.map((link) => link.speed)));
</script>

<template>
  <section aria-label="Network links">
    <div class="total">
      <div class="figure">
        <strong class="num">{{ formatSpeed(engine.totalSpeed) }}</strong>
        <span class="muted">across {{ enabledCount }} link{{ enabledCount === 1 ? "" : "s" }}</span>
      </div>
      <ThroughputBar :links="engine.links" />
    </div>
    <p v-if="engine.links.length === 0" class="empty muted">
      No usable network link found yet.
    </p>
    <ul v-else class="cards">
      <LinkCard v-for="link in engine.links" :key="link.name" :link="link" :peak="peak" />
    </ul>
  </section>
</template>

<style scoped>
.total {
  display: grid;
  gap: 10px;
  padding: 14px 16px;
  margin-bottom: 14px;
  border: 1px solid var(--border);
  border-radius: 10px;
  background: var(--surface);
}

.figure {
  display: flex;
  align-items: baseline;
  gap: 12px;
}

.figure strong {
  font-size: 20px;
}

.cards {
  margin: 0;
  padding: 0;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 10px;
}

.empty {
  text-align: center;
  padding: 48px 0;
}
</style>
