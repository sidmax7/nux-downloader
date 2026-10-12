<script setup lang="ts">
import { computed } from "vue";
import { formatSpeed } from "../format";
import { useEngineStore } from "../stores/engine";
import LinkCard from "./LinkCard.vue";

const engine = useEngineStore();
const peak = computed(() => Math.max(0, ...engine.links.map((link) => link.speed)));
</script>

<template>
  <section aria-label="Network links">
    <p class="muted intro">
      Each network link downloads at once. Together:
      <strong class="num">{{ formatSpeed(engine.totalSpeed) }}</strong>
    </p>
    <p v-if="engine.links.length === 0" class="empty muted">
      No usable network link found yet.
    </p>
    <ul v-else class="cards">
      <LinkCard v-for="link in engine.links" :key="link.name" :link="link" :peak="peak" />
    </ul>
  </section>
</template>

<style scoped>
.intro {
  margin: 0 0 12px;
}

.cards {
  margin: 0;
  padding: 0;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
  gap: 10px;
}

.empty {
  text-align: center;
  padding: 48px 0;
}
</style>
